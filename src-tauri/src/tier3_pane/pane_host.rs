//! Single-window GTK/wgpu pane host -- multi-pane, on-demand (items.id=202
//! piece 5, items.id=223), single-window compositing (items.id=202 real
//! positioning fix, 2026-08-07).
//!
//! # What this replaces, and why
//! The prior design (this file, formerly `sync_window.rs`) gave every open
//! pane its own separate `winit::Window` and `wgpu::Surface`, manually kept
//! in sync with the main Tauri window via `window.set_outer_position()`/
//! `request_inner_size()`, driven by `window.inner_position()`/`inner_size()`
//! queried from the main window on every native `Moved`/`Resized` event
//! (main.rs). That query is confirmed broken on Wayland (manual verification,
//! 2026-08-07: pinned at a fixed, wrong `x=0,y=0` plus an oversized rect on
//! this dev machine's KDE/Wayland session -- consistent with the real
//! upstream issues tauri-apps/tauri#12411 and tauri-apps/tao#566). The
//! per-pane fraction math itself (`paneLayout.ts`, `PaneLayoutState`) was
//! never the problem and is unchanged by this rewrite.
//!
//! The real fix: every open pane now composites into ONE shared `gtk::GLArea`
//! widget, overlaid on top of Tauri's own webview widget inside Tauri's own
//! window (obtained via `tauri::WebviewWindow::gtk_window()`/`default_vbox()`)
//! -- not a separate OS window at all. wgpu presents into that GLArea's own
//! GL framebuffer via wgpu-hal's external-GL-context interop
//! (`wgpu_hal::gles::Adapter::new_external`, see render.rs), proven working
//! this session (matched `glReadPixels` across 19 frames, direct visual
//! confirmation of a wgpu-rendered pane composited over a real webview in one
//! window). This is not a workaround for the Wayland bug -- it deletes the
//! code path that has the bug. Window move/resize now costs zero Rust-side
//! geometry-query code: GTK relayouts the GLArea for free as an ordinary
//! child widget.
//!
//! **Linux only.** `gtk::GLArea` has no equivalent on Windows (WebView2/HWND)
//! or macOS (WKWebView/NSWindow) -- this reverses the "one manually-synced
//! architecture across all platforms" policy the old design in this file
//! stated, for Linux specifically. Every proven fact in this whole track
//! (isolation, native Wayland, NVIDIA/GBM, popup delegation) is Linux-only;
//! Windows/macOS need their own equivalent single-window host mechanism,
//! not designed here -- same practice as the rest of this track (ship the
//! Linux-verified result, scope other platforms as explicit future work).
//!
//! # winit is gone from `tier3_pane` entirely
//! CEF's panes were never windowed to begin with
//! (`windowless_rendering_enabled: true` -- true off-screen rendering, per
//! decisions.id=699). The old per-pane `winit::Window` existed only to give
//! wgpu a presentation surface and an OS window to position-sync -- once
//! wgpu presents into GTK's own GLArea framebuffer instead, that whole layer
//! (`winit::Window`, `winit::EventLoop`, `EventLoopProxy`,
//! `ApplicationHandler`) has no remaining job. This also structurally
//! obsoletes the freeze-bug heartbeat hack that used to live in main.rs (a
//! 16ms `run_on_main_thread(|| {})` loop whose entire purpose was forcing
//! `tao`'s own blocking GTK loop to notice a second, GTK-invisible
//! winit/Wayland connection). With no second event loop, there is no second
//! connection for `tao` to be blind to -- GTK's own main loop already owns
//! the GLArea's draw cycle as a first-class `GSource` (the periodic
//! `glib::timeout_add_local` below, and the GLArea's own `render`/`resize`
//! signals). This is the expected, structural consequence of removing the
//! thing that caused the freeze bug -- worth verifying under sustained use,
//! not just asserting from the argument (see the harness's own manual
//! verification).
//!
//! # What's unchanged from the old design
//! `BrowserLifecycle`/`PendingAction`/`apply_action` (CEF browser
//! creation/navigation state machine), `PaneCommand::{Open,Close}`'s meaning,
//! `PaneKey` (provider ID), the on-demand lifecycle (items.id=223 -- nothing
//! is created until `PaneHost::open_pane` is called), and every CEF-facing
//! piece of render.rs (`PaneRenderHandler`, paint callbacks, `PANE_TEXTURES`,
//! `ClientBuilder`, `LifeSpanHandler`) are all untouched -- already correctly
//! keyed by `PaneKey`, already orthogonal to windowing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

// DIAG items.id=312 (temporary -- remove after root-cause diagnosis):
// resize/render burst investigation. Extends the existing DIAG
// items.id=227 logging with millisecond-resolution timestamps and a
// shared monotonic sequence number so a `connect_resize`/`connect_render`
// burst can be reconstructed in order across both handlers from the log
// alone.
static DIAG_312_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
static DIAG_312_SEQ: AtomicU64 = AtomicU64::new(0);
fn diag_312_tick() -> (u64, u128) {
    (
        DIAG_312_SEQ.fetch_add(1, Ordering::Relaxed),
        DIAG_312_START.elapsed().as_millis(),
    )
}

use gtk::prelude::*;
use indexmap::IndexMap;
use tauri::Manager;

use cef::{
    ImplBrowser, ImplBrowserHost, ImplFrame, KeyEvent, KeyEventType, MouseButtonType, MouseEvent,
};

use crate::commands::tier3_pane::{
    PaneEventModifiers, PaneKeyEventType, PaneLayoutState, PaneMouseButton, PaneRectFraction,
    PopupClosedPayload, PopupOpenedPayload,
};
use crate::tier3_pane::render::{
    ClientBuilder, LogicalSize, PaneRenderHandler, PopupLifecycleEvent, PopupRequested,
    RenderState,
};
use crate::tier3_pane::PaneKey;

/// Lifecycle of one pane's CEF browser -- unchanged from the prior design.
#[allow(dead_code)] // Closing/Closed: no code path drives these yet --
                    // close_pane goes straight from Ready to removing the
                    // PaneState entirely rather than passing through this
                    // enum's own Closing/Closed states. Kept per the
                    // finalized design; wire them if a graceful (non-forced)
                    // close path is ever needed.
enum BrowserLifecycleState {
    Uninitialized,
    Creating,
    Ready(cef::Browser),
    Closing,
    Closed,
    Failed(String),
}

/// Deferred until the browser is `Ready`; applied immediately if it already
/// is. Failure policy: every failed apply gets an unconditional log::warn! --
/// no retry, no completion signal.
#[derive(Debug)]
#[allow(dead_code)] // SetCookie: explicit stub -- no cookie-manager API is
                    // wired at this layer (cookie persistence is handled at
                    // the commands::tier3_pane layer, around this pane's
                    // open/close, not per-action here).
enum PendingAction {
    Navigate(String),
    SetCookie { name: String, value: String },
    /// items.id=359 piece 5: fire-and-forget JS run via
    /// `Frame::execute_java_script` -- currently only used for the blind
    /// scroll-to-bottom-on-deactivation trigger (a generic
    /// `window.scrollTo`, no per-provider knowledge), but kept as a
    /// general string-of-JS variant rather than a single-purpose
    /// `ScrollToBottom` marker, matching `Navigate`'s own precedent of
    /// carrying the actual payload rather than a symbolic action name.
    ExecuteScript(String),
    /// items.id=359 piece 4: the actual "deactivated/minimized pane"
    /// mechanism -- CEF's own hook for "keep this browser's state alive
    /// but stop painting/compositing it" (`ImplBrowserHost::was_hidden`).
    /// Distinct from `Close`: a hidden pane keeps its `BrowserLifecycle`,
    /// its CEF `Browser`, and (per `PaneManager::active_pane`'s own doc)
    /// stays reachable to reactivate without reloading.
    SetHidden(bool),
}

struct BrowserLifecycle {
    state: BrowserLifecycleState,
    pending: Vec<PendingAction>,
}

impl BrowserLifecycle {
    fn new() -> Self {
        Self {
            state: BrowserLifecycleState::Uninitialized,
            pending: Vec::new(),
        }
    }

    /// `true` (and transitions to `Creating`) only the first time this is
    /// called while `Uninitialized`.
    fn start_creation(&mut self) -> bool {
        if matches!(self.state, BrowserLifecycleState::Uninitialized) {
            self.state = BrowserLifecycleState::Creating;
            true
        } else {
            false
        }
    }

    fn fail(&mut self, reason: impl Into<String>) {
        self.state = BrowserLifecycleState::Failed(reason.into());
    }

    /// Called once this pane's `browser_ready_rx` yields a `Browser` --
    /// transitions to `Ready` and drains anything queued while
    /// `Uninitialized`/`Creating`.
    fn on_created(&mut self, browser: cef::Browser) {
        self.state = BrowserLifecycleState::Ready(browser.clone());
        for action in self.pending.drain(..) {
            if let Err(e) = apply_action(&browser, &action) {
                log::warn!("tier3_pane: queued action failed on drain: {action:?}: {e}");
            }
        }
    }

    fn enqueue(&mut self, action: PendingAction) {
        match &self.state {
            BrowserLifecycleState::Ready(browser) => {
                if let Err(e) = apply_action(browser, &action) {
                    log::warn!("tier3_pane: action failed immediately: {action:?}: {e}");
                }
            }
            _ => self.pending.push(action),
        }
    }

    fn browser(&self) -> Option<&cef::Browser> {
        match &self.state {
            BrowserLifecycleState::Ready(browser) => Some(browser),
            _ => None,
        }
    }
}

fn apply_action(browser: &cef::Browser, action: &PendingAction) -> Result<(), String> {
    match action {
        PendingAction::Navigate(url) => {
            let Some(frame) = browser.main_frame() else {
                return Err("browser has no main_frame yet".to_string());
            };
            frame.load_url(Some(&cef::CefString::from(url.as_str())));
            Ok(())
        }
        PendingAction::SetCookie { .. } => {
            Err("SetCookie not yet supported at this layer".to_string())
        }
        PendingAction::ExecuteScript(code) => {
            let Some(frame) = browser.main_frame() else {
                return Err("browser has no main_frame yet".to_string());
            };
            frame.execute_java_script(Some(&cef::CefString::from(code.as_str())), None, 0);
            Ok(())
        }
        PendingAction::SetHidden(hidden) => {
            let Some(host) = browser.host() else {
                return Err("browser has no host yet".to_string());
            };
            host.was_hidden(*hidden as _);
            Ok(())
        }
    }
}

/// items.id=359 piece 5: the blind scroll-to-bottom trigger fired on a
/// pane's own deactivation (`PaneManager::set_active_pane`) -- generic,
/// no per-provider DOM knowledge, confirmed feasible via
/// `Frame::execute_java_script` (items.id=358, handoff id=245). "Blind"
/// deliberately: landing precisely on the last chat response (rather
/// than just the bottom of the page) shares the response-detection
/// question's DOM-introspection dependency, which is explicitly NOT part
/// of this item's scope (see items.id=360).
const SCROLL_TO_BOTTOM_JS: &str = "window.scrollTo(0, document.body.scrollHeight);";

/// The two operations `PaneHost::open_pane`/`close_pane` perform, dispatched
/// via `AppHandle::run_on_main_thread` from `commands::tier3_pane`'s async
/// IPC handlers -- kept as a named enum (rather than inlining two separate
/// methods at each call site) purely for the same call-site clarity/logging
/// the old winit-user-event design had, even though there is no event queue
/// to dispatch through anymore.
#[derive(Debug, Clone)]
pub enum PaneCommand {
    Open { key: PaneKey, url: String },
    Close { key: PaneKey },
    /// items.id=359 pieces 4/5: makes `key` (or none) the one pane that's
    /// actually composited/painted -- see `PaneManager::active_pane`'s own
    /// doc for the single-active-pane invariant this enforces. The
    /// outgoing active pane (if any, and if different from `key`) gets a
    /// scroll-to-bottom trigger fired just before it's hidden.
    SetActivePane { key: Option<PaneKey> },
    /// DOM-forwarded mouse press/release (items.id=257 Path B -- see the
    /// "Pane content click/mouse forwarding" section doc above). `x`/`y`
    /// arrive already pane-local, in CEF-logical pixels -- see
    /// `forward_pane_mouse_click`'s own doc (commands/tier3_pane.rs).
    MouseClick {
        key: PaneKey,
        x: f64,
        y: f64,
        button: PaneMouseButton,
        mouseup: bool,
        click_count: i32,
        buttons: u16,
        modifiers: PaneEventModifiers,
    },
    MouseMove {
        key: PaneKey,
        x: f64,
        y: f64,
        leaving: bool,
        buttons: u16,
        modifiers: PaneEventModifiers,
    },
    MouseWheel {
        key: PaneKey,
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: PaneEventModifiers,
    },
    /// items.id=332: `windows_key_code`/`character` are already resolved
    /// from the DOM `KeyboardEvent` on the frontend side (see
    /// `forward_pane_key`'s own doc, commands/tier3_pane.rs) -- this arm's
    /// job is only to build CEF's `KeyEvent` and call `send_key_event`.
    KeyEvent {
        key: PaneKey,
        event_type: PaneKeyEventType,
        windows_key_code: i32,
        character: u16,
        modifiers: PaneEventModifiers,
    },
    /// items.id=234: same shape as `MouseClick`/`MouseMove`/`MouseWheel`
    /// above, which are left untouched -- `key` is the *parent* pane's key
    /// (popups have no separate id-keyspace, see `PaneManager.popups`'s own
    /// doc), `x`/`y` are local to the popup's own on-screen rect, not the
    /// parent pane's.
    PopupMouseClick {
        key: PaneKey,
        x: f64,
        y: f64,
        button: PaneMouseButton,
        mouseup: bool,
        click_count: i32,
        buttons: u16,
        modifiers: PaneEventModifiers,
    },
    PopupMouseMove {
        key: PaneKey,
        x: f64,
        y: f64,
        leaving: bool,
        buttons: u16,
        modifiers: PaneEventModifiers,
    },
    PopupMouseWheel {
        key: PaneKey,
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        modifiers: PaneEventModifiers,
    },
}

/// Per-pane state. No `window`/per-pane `render_state` fields (one shared
/// `RenderState` for the whole app now, owned by `PaneHost` -- see its own
/// doc). No `last_requested_size`/maximize-fighting guard either -- that
/// existed to stop a separate OS window's `sync_to()` calls from fighting a
/// user who manually maximized/tiled/resized that window directly; there is
/// no second OS window here for a user to grab.
struct PaneState {
    browser_lifecycle: BrowserLifecycle,
    // Arc<Mutex<>>, not Rc<RefCell<>> -- shared with PaneRenderHandler, whose
    // view_rect runs on CEF's UI thread while this pane's own resize-sync
    // (PaneHost's GLArea render/resize callbacks, main thread) writes it.
    // See PaneRenderHandler::size docs (render.rs) for why this must not be
    // an Rc<RefCell<>> under multi_threaded_message_loop=true (items.id=203
    // audit, 2026-08-03) -- unchanged reasoning, still two real OS threads.
    browser_size: Arc<Mutex<LogicalSize>>,
    /// Receives the constructed `Browser` from `LifeSpanHandler::on_after_created`
    /// (see render.rs docs) -- CEF's UI thread delivers it here since
    /// creation is async under multi_threaded_message_loop. Drained on every
    /// GLArea `render` tick (see `PaneHost::install`), same role as the old
    /// design's per-tick `pump()`.
    browser_ready_rx: Option<std::sync::mpsc::Receiver<cef::Browser>>,
    /// The physical pixel size (whole window content area x this pane's own
    /// `PaneRectFraction`) last applied to `browser_size`/CEF's
    /// `was_resized()`. Compared against the freshly-computed size on every
    /// render tick so `was_resized()` is only called on an actual change,
    /// not once per frame unconditionally -- the per-pane equivalent of the
    /// old design's `RESIZE_DEBOUNCE`, but a plain equality check rather
    /// than a timer: there is no external window-manager gesture to
    /// coalesce here, just this app's own layout math re-running each tick.
    last_applied_size: Option<(u32, u32)>,
}

/// items.id=234: lifecycle of one popup's CEF browser. Unlike
/// `BrowserLifecycleState` (panes), there is no `Closing`/`Closed`
/// mid-state here -- a popup goes straight from `Creating` to being removed
/// from `PaneManager.popups` entirely (`force_close_popup`), same as
/// `close_pane` already does for panes.
enum PopupLifecycleState {
    Creating,
    Ready(cef::Browser),
    /// No producer wires this yet (`on_before_popup_aborted`, the CEF
    /// signal for a popup that failed to construct, is not hooked up in
    /// this item's scope) -- reserved should that ever be added, same
    /// forward-compatible-but-currently-dead-code precedent as
    /// `BrowserLifecycleState::Closing`/`Closed` above.
    #[allow(dead_code)]
    Failed(String),
}

impl PopupLifecycleState {
    fn browser(&self) -> Option<&cef::Browser> {
        match self {
            PopupLifecycleState::Ready(browser) => Some(browser),
            _ => None,
        }
    }
}

/// items.id=234: one open popup's bookkeeping -- parallel to `PaneState`,
/// but much simpler (a popup never navigates on the host's behalf, has no
/// `PendingAction` queue, and is force-closed rather than gracefully torn
/// down, see `force_close_popup`).
struct PopupState {
    lifecycle: PopupLifecycleState,
    /// Drained once per GTK render tick (`drain_popup_events`), same role
    /// as `PaneState.browser_ready_rx` -- see `PopupLifecycleEvent`'s own
    /// doc (render.rs) for why this needs two variants where a pane's
    /// channel only ever needed one.
    events_rx: std::sync::mpsc::Receiver<PopupLifecycleEvent>,
    /// Shared with `PopupRenderHandler`'s `view_rect` (CEF's UI thread) --
    /// same `Arc<Mutex<>>` reasoning as `PaneState.browser_size`.
    size: Arc<Mutex<LogicalSize>>,
    /// This popup's on-screen rect, as a fraction of the whole GLArea --
    /// resolved once at creation (`resolve_popup_rect`) and never
    /// recomputed from anything the page itself requests (items.id=234
    /// plan, Judgment call 6.5: no page-initiated popup resize support).
    rect: PaneRectFraction,
    last_applied_size: Option<(u32, u32)>,
}

/// One notification `drain_popup_requests`/`drain_popup_events`/
/// `drain_popup_close_requests` produced this tick, for `PaneHost::install`'s
/// `connect_render` closure (which has `app_handle` in scope) to turn into
/// an actual `AppHandle::emit` call -- kept out of `PaneManager` itself so
/// this plain-data struct stays free of any Tauri dependency, matching
/// `sync_pane_sizes`/`drain_ready_browsers`'s own existing convention of
/// taking only what they need (never `AppHandle`) and leaving IPC/event
/// concerns to their caller.
enum PopupNotification {
    Opened { key: PaneKey, rect: PaneRectFraction },
    Closed { key: PaneKey },
}

/// One shared render target's worth of pane bookkeeping. Not a
/// `winit::ApplicationHandler` anymore -- a plain struct, driven by GTK's
/// `GLArea` signals instead of a winit event loop.
struct PaneManager {
    /// `IndexMap`, not `HashMap`: insertion order == pane open order, useful
    /// for anything that wants a stable display order (e.g. the frontend's
    /// own column assignment). No `window_ids` reverse lookup anymore --
    /// there's only one real window now, no per-pane `WindowId` to dispatch
    /// on.
    panes: IndexMap<PaneKey, PaneState>,
    /// Live count -- incremented on `Open`, decremented on `Close`. Shared
    /// with main.rs's GLib timeout so it can gate the redraw heartbeat on
    /// whether any pane is actually open right now (items.id=223).
    open_pane_count: Arc<AtomicUsize>,
    /// Set on pointer-down inside a pane's rect, resolved by the frontend's
    /// own DOM hit-testing and forwarded via `forward_pane_mouse_click`
    /// (items.id=257, Path B redesign -- see `PaneHost::dispatch`'s
    /// `PaneCommand::MouseClick` arm). This field exists
    /// for the input class hit-testing *can't* resolve on its own: keyboard
    /// events have no coordinate to test against, so a future
    /// `send_key_event` forward needs an explicit answer to "which pane."
    /// Not consumed anywhere yet -- keyboard forwarding is out of this
    /// session's scope (click/mouse forwarding only); tracked now so wiring
    /// it later is a small addition, not a redesign.
    #[allow(dead_code)]
    focused_pane: Option<PaneKey>,
    /// items.id=234: one active popup per pane (Judgment call 6.1 -- OAuth
    /// login is inherently single-popup; opening a second for the same
    /// pane replaces the first, see `drain_popup_requests`), keyed by the
    /// *parent* pane's `PaneKey`, not a separate popup-id keyspace.
    popups: HashMap<PaneKey, PopupState>,
    /// Fed by `PaneLifeSpanHandler::on_before_popup` (render.rs, CEF's UI
    /// thread) via `popup_requested_tx`'s paired sender, cloned into each
    /// pane's `PaneLifeSpanHandler` at `open_pane` time. Drained once per
    /// GTK render tick (`drain_popup_requests`), which does have the GTK
    /// main thread's own `glarea_size` needed to resolve the popup's rect.
    popup_requested_tx: std::sync::mpsc::Sender<PopupRequested>,
    popup_requested_rx: std::sync::mpsc::Receiver<PopupRequested>,
    /// Fed by `PaneLoadHandler::on_load_start` (render.rs, CEF's UI thread,
    /// main-frame navigations of a PARENT pane's own browser only -- see
    /// that handler's doc for why this is structurally never fed by a
    /// popup's own navigation) via `popup_close_tx`'s paired sender, cloned
    /// into each pane's `PaneLoadHandler` at `open_pane` time. Drained once
    /// per GTK render tick (`drain_popup_close_requests`).
    popup_close_tx: std::sync::mpsc::Sender<PaneKey>,
    popup_close_rx: std::sync::mpsc::Receiver<PaneKey>,
    /// items.id=359 (rail+content-pane redesign): the one pane, if any,
    /// that is actually composited/painted right now -- every other open
    /// pane is "loaded" (browser alive, `was_hidden(true)`, reachable
    /// without a reload) but shown nowhere. This is the real invariant
    /// the rail model enforces ("only one provider is ever rendered at
    /// full size at a time"), tracked centrally here rather than as a
    /// per-`PaneState` boolean, since "which one" is the whole point, not
    /// an independent per-pane fact. Updated only by `set_active_pane`
    /// and, defensively, by `close_pane` (closing the active pane clears
    /// this rather than leaving it dangling).
    active_pane: Option<PaneKey>,
    /// items.id=334: needed to construct each pane/popup's render handler
    /// with a `request_redraw` (render.rs) capability -- see its own doc.
    app_handle: tauri::AppHandle,
}

impl PaneManager {
    /// Opens a pane and dispatches CEF browser creation immediately -- no
    /// more "wait for this pane's first window redraw" deferral. That
    /// deferral existed because Phase A/B's per-pane browser creation needed
    /// this pane's own `Window` to exist first; under single-window
    /// compositing the shared `RenderState`/`GLArea` already exists (built
    /// once, at GTK realize time, before any pane can open at all -- see
    /// `PaneHost::install`), so the old precondition for deferring is gone.
    fn open_pane(
        &mut self,
        key: PaneKey,
        url: String,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        device_scale_factor: f32,
        initial_logical_size: LogicalSize,
    ) {
        if self.panes.contains_key(&key) {
            log::warn!(
                "tier3_pane::pane_host: open requested for already-open pane {key:?} -- ignoring"
            );
            return;
        }

        // accelerated_osr is always enabled for the `cef` dependency (see
        // Cargo.toml) -- the platform check alone determines whether the
        // shared-texture path is used vs. the software on_paint fallback.
        let accelerated_osr = cfg!(any(
            target_os = "macos",
            target_os = "windows",
            target_os = "linux"
        ));
        let window_info = cef::WindowInfo {
            windowless_rendering_enabled: true as _,
            // items.id=257 root cause (2026-08-28): the `cef` crate's Linux
            // DMA-BUF importer (osr_texture_import/dmabuf.rs,
            // `DmaBufImporter::supports_hardware_acceleration`) only ever
            // succeeds when the wgpu `Device` is Vulkan-backed
            // (`vulkan::is_vulkan_backend`) -- this app's `Device` is always
            // GLES-backed (`wgpu_hal::gles::Adapter::new_external`, required
            // to interop with GTK's own native GL context), so that check
            // always fails and it silently falls back to
            // `texture::create_fallback`, a freshly-allocated,
            // NEVER-WRITTEN-TO (all-zero/black) placeholder texture -- no
            // error, no warning, just a permanently blank pane. Confirmed
            // live: forcing the fragment shader's output alpha to 1.0
            // revealed solid black, not real page content. `shared_texture_
            // enabled = false` routes CEF into its software OSR path
            // instead (`on_paint`, real CPU pixel buffer), which this
            // codebase already fully implements (`PendingPaint::Software` ->
            // `resolve_bind_group`'s `queue.write_texture` call) -- it was
            // simply unreachable while shared-texture mode was on, since CEF
            // never calls `on_paint` once `shared_texture_enabled` is true.
            shared_texture_enabled: false as _,
            external_begin_frame_enabled: accelerated_osr as _,
            ..Default::default()
        };
        let browser_settings = cef::BrowserSettings {
            windowless_frame_rate: 60,
            ..Default::default()
        };

        let (render_handler, browser_size) = PaneRenderHandler::new(
            device.clone(),
            queue.clone(),
            device_scale_factor,
            initial_logical_size,
            key.clone(),
            self.app_handle.clone(),
        );

        // No per-pane RequestContext (items.id=224 resolution,
        // decisions.id=711): CEF's Chrome-runtime ChromeBrowserContext
        // structurally rejects any second RequestContext -- every pane uses
        // CEF's one working global context (request_context=None below).
        // Per-provider cookie *persistence* across app restarts is handled
        // at the QR application layer instead -- see
        // commands/tier3_pane.rs::open_tier3_panes/close_tier3_pane.
        let mut browser_lifecycle = BrowserLifecycle::new();
        let (tx, rx) = std::sync::mpsc::channel();
        browser_lifecycle.start_creation();
        let created = cef::browser_host_create_browser(
            Some(&window_info),
            Some(&mut ClientBuilder::build(
                render_handler,
                tx,
                key.clone(),
                self.popup_requested_tx.clone(),
                self.popup_close_tx.clone(),
                device.clone(),
                queue.clone(),
                self.app_handle.clone(),
            )),
            None,
            Some(&browser_settings),
            None,
            None,
        );
        log::info!(
            "tier3_pane::pane_host: browser_host_create_browser (async, pane={key}) dispatched -> {created}"
        );
        if created != 0 {
            browser_lifecycle.enqueue(PendingAction::Navigate(url));
            // items.id=359 piece 4: a newly-opened pane starts deactivated
            // by default -- the caller (commands::tier3_pane::open_tier3_panes)
            // always follows Open with an explicit set_active_pane call for
            // the "idle row -> load and activate in one step" case, but
            // defaulting to hidden here (rather than assuming the caller's
            // next call arrives before this pane's first paint) means there
            // is no window where a pane composites before this item's rail
            // model has actually decided it should. Queued the same way
            // Navigate is -- applied immediately if the browser is already
            // Ready, deferred otherwise.
            browser_lifecycle.enqueue(PendingAction::SetHidden(true));
        } else {
            browser_lifecycle.fail(
                "browser_host_create_browser returned false (async creation dispatch failed)",
            );
        }

        self.panes.insert(
            key,
            PaneState {
                browser_lifecycle,
                browser_size,
                browser_ready_rx: Some(rx),
                last_applied_size: None,
            },
        );
        self.open_pane_count.fetch_add(1, Ordering::Relaxed);
    }

    fn close_pane(&mut self, key: &PaneKey) {
        // items.id=234: force-close this pane's own popup, if any, before
        // its browser goes away -- must run before shift_remove below,
        // since force_close_popup's own defensive parent-grab-release
        // still needs to find this pane in self.panes.
        self.force_close_popup(key);

        let Some(pane) = self.panes.shift_remove(key) else {
            return;
        };
        // Forced close (no unload-confirmation dance) -- a user-initiated
        // pane close (items.id=223's whole trigger), not a navigation-away
        // the page itself might want to intercept.
        if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
            // items.id=313: release CEF's own focus state before the
            // browser goes away, if this pane was the one holding it.
            if self.focused_pane.as_ref() == Some(key) {
                host.set_focus(false as _);
            }
            host.close_browser(true as _);
        }
        crate::tier3_pane::render::remove_pane_texture(key);
        crate::tier3_pane::render::remove_pane_pending_paint(key);
        self.open_pane_count.fetch_sub(1, Ordering::Relaxed);
        if self.focused_pane.as_ref() == Some(key) {
            self.focused_pane = None;
        }
        // items.id=359: a closed active pane must not leave active_pane
        // dangling -- no scroll-to-bottom trigger needed here (unlike
        // set_active_pane's own deactivation path), since this pane is
        // gone, not being minimized for later reactivation.
        if self.active_pane.as_ref() == Some(key) {
            self.active_pane = None;
        }
    }

    /// items.id=359 pieces 4/5: makes `key` (or `None`) the one pane that's
    /// actually composited -- see `active_pane`'s own doc for the
    /// single-active-pane invariant. A no-op if `key` already equals the
    /// current `active_pane` (switching a pane to itself, e.g. the content
    /// pane's own click-to-reclaim-focus gesture, still needs `was_hidden`
    /// re-asserted false in case QR's own re-expand had hidden it in the
    /// meantime -- so this is NOT gated on equality; see below).
    fn set_active_pane(&mut self, key: Option<PaneKey>) {
        let previous = self.active_pane.clone();
        if previous != key {
            if let Some(old_key) = previous {
                if let Some(pane) = self.panes.get_mut(&old_key) {
                    // Scroll-to-bottom fires on the OUTGOING pane, before
                    // it's hidden, so reactivating it later lands back on
                    // its last response rather than wherever it was
                    // scrolled (items.id=359 piece 5).
                    pane.browser_lifecycle
                        .enqueue(PendingAction::ExecuteScript(SCROLL_TO_BOTTOM_JS.to_string()));
                    pane.browser_lifecycle
                        .enqueue(PendingAction::SetHidden(true));
                }
            }
        }
        if let Some(new_key) = &key {
            if let Some(pane) = self.panes.get_mut(new_key) {
                pane.browser_lifecycle
                    .enqueue(PendingAction::SetHidden(false));
            }
        }
        self.active_pane = key;
    }

    /// Drains any browser CEF's UI thread finished constructing since the
    /// last tick (see render.rs's LifeSpanHandler docs). Called once per
    /// GLArea `render` tick.
    fn drain_ready_browsers(&mut self) {
        for pane in self.panes.values_mut() {
            if let Some(rx) = pane.browser_ready_rx.as_ref() {
                if let Ok(browser) = rx.try_recv() {
                    log::info!("tier3_pane::pane_host: browser delivered via on_after_created");
                    pane.browser_lifecycle.on_created(browser);
                }
            }
        }
    }

    /// Recomputes every open pane's target pixel size from `layout`
    /// (`PaneLayoutState`'s live fractions) x `glarea_size` (the GLArea's
    /// own physical size, which already *is* the window's content area,
    /// since the GLArea fills it -- see render.rs's `render()` doc for why
    /// no `PhysicalRect`/window-position query is needed here at all), and
    /// pushes `was_resized()` to CEF only for panes whose size actually
    /// changed since the last tick. A pane with no reported layout yet is
    /// left alone, not resized to a placeholder -- matches the old design's
    /// same choice.
    fn sync_pane_sizes(
        &mut self,
        glarea_size: (u32, u32),
        layout: &HashMap<PaneKey, PaneRectFraction>,
    ) {
        for (key, pane) in self.panes.iter_mut() {
            let Some(frac) = layout.get(key) else {
                continue;
            };
            let Some((_, _, width, height)) = pane_pixel_rect(glarea_size, frac) else {
                continue;
            };
            let size_px = (width, height);
            if pane.last_applied_size == Some(size_px) {
                continue;
            }
            // items.id=328: `LogicalSize` here feeds CEF's `GetViewRect`
            // directly (render.rs's `view_rect`) -- confirmed live, via
            // temporary DIAG logging, that CEF's actual OSR paint buffer
            // comes back at exactly this reported size, NOT this size x
            // `device_scale_factor`. Dividing by `scale_factor` before
            // reporting (the previous behavior) told CEF to render at half
            // (or 1/scale) the real physical pixel count, which this app's
            // own draw step then stretched back up to the full destination
            // rect -- the actual root cause of this item's blur, not a minor
            // rounding gap. Report the real physical size directly; the
            // separate `device_scale_factor` reported via `GetScreenInfo`
            // is what tells the page's own layout/DPI math (not this
            // buffer's pixel count) to treat it as a scaled display.
            *pane.browser_size.lock().unwrap() = LogicalSize {
                width: width as f32,
                height: height as f32,
            };
            if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
                host.was_resized();
            }
            pane.last_applied_size = Some(size_px);
        }
    }

    /// items.id=234 counterpart to `sync_pane_sizes`, for open popups --
    /// same mechanism (a popup's rect is a fixed fraction of the GLArea,
    /// rescaled proportionally on ordinary window resize; see this item's
    /// plan, Judgment call 6.5), no new CEF hook needed since a popup is a
    /// genuinely separate `Browser` (unlike `on_popup_size`, which targets
    /// the different same-browser dropdown-popup mechanism -- see
    /// render.rs's "items.id=234" section doc).
    fn sync_popup_sizes(&mut self, glarea_size: (u32, u32)) {
        for popup in self.popups.values_mut() {
            let Some((_, _, width, height)) = pane_pixel_rect(glarea_size, &popup.rect) else {
                continue;
            };
            let size_px = (width, height);
            if popup.last_applied_size == Some(size_px) {
                continue;
            }
            // items.id=328: same fix as `sync_pane_sizes` above -- see its
            // comment.
            *popup.size.lock().unwrap() = LogicalSize {
                width: width as f32,
                height: height as f32,
            };
            if let Some(host) = popup.lifecycle.browser().and_then(|b| b.host()) {
                host.was_resized();
            }
            popup.last_applied_size = Some(size_px);
        }
    }

    /// items.id=234: force-closes `key`'s popup, if one is open -- shared by
    /// `close_pane` (parent pane closing), `drain_popup_requests` (a second
    /// popup request replaces the first, Judgment call 6.1), and
    /// `drain_popup_close_requests` (parent navigate-away). Includes the
    /// defensive grab-release this item's plan calls for (Judgment call
    /// 6.6) -- unconditional, not gated on first confirming the
    /// `ITEMS257_INPUT_FREEZE_INVESTIGATION_20260822.md` stuck-grab
    /// hypothesis; cheap and idempotent if nothing was actually grabbed.
    fn force_close_popup(&mut self, key: &PaneKey) {
        let Some(popup) = self.popups.remove(key) else {
            return;
        };
        if let Some(host) = popup.lifecycle.browser().and_then(|b| b.host()) {
            host.send_capture_lost_event();
            host.close_browser(true as _);
        }
        crate::tier3_pane::render::remove_popup_texture(key);
        crate::tier3_pane::render::remove_popup_pending_paint(key);

        if let Some(host) = self
            .panes
            .get(key)
            .and_then(|p| p.browser_lifecycle.browser())
            .and_then(|b| b.host())
        {
            host.send_capture_lost_event();
        }
        if let Some(seat) = gtk::gdk::Display::default().and_then(|d| d.default_seat()) {
            seat.ungrab();
        }
    }

    /// items.id=234: resolves every `PopupRequested` CEF's UI thread queued
    /// since the last tick into a real `PopupState`, using the GTK main
    /// thread's own live `glarea_size`/`layout` (unavailable to the
    /// `on_before_popup` callback that produced the request -- see that
    /// handler's own doc). `layout` is `PaneLayoutState`'s live contents,
    /// same as `sync_pane_sizes` already takes, needed here only to look up
    /// the requesting pane's own current pixel rect for centering.
    fn drain_popup_requests(
        &mut self,
        glarea_size: (u32, u32),
        layout: &HashMap<PaneKey, PaneRectFraction>,
    ) -> Vec<PopupNotification> {
        let mut requests = Vec::new();
        while let Ok(req) = self.popup_requested_rx.try_recv() {
            requests.push(req);
        }

        let mut notifications = Vec::new();
        for req in requests {
            // One popup per pane (Judgment call 6.1): a second request for
            // an already-open popup replaces the first.
            self.force_close_popup(&req.parent_key);

            let parent_pixel_rect = layout
                .get(&req.parent_key)
                .and_then(|frac| pane_pixel_rect(glarea_size, frac));
            let rect = resolve_popup_rect(glarea_size, parent_pixel_rect, &req.features);

            self.popups.insert(
                req.parent_key.clone(),
                PopupState {
                    lifecycle: PopupLifecycleState::Creating,
                    events_rx: req.events_rx,
                    size: req.size,
                    rect,
                    last_applied_size: None,
                },
            );
            notifications.push(PopupNotification::Opened {
                key: req.parent_key,
                rect,
            });
        }
        notifications
    }

    /// items.id=234: drains each open popup's own lifecycle channel (`Ready`
    /// from either capture path -- see `PopupRenderHandler`'s dual-capture
    /// doc; `Closed` from the popup self-closing, e.g. `window.close()`
    /// after a completed OAuth login). At most one event per popup per
    /// tick, same as `drain_ready_browsers`' own per-pane `try_recv` --
    /// events are infrequent enough that a queued second event simply
    /// resolves on the next tick.
    fn drain_popup_events(&mut self) -> Vec<PopupNotification> {
        let mut closed_keys = Vec::new();
        for (key, popup) in self.popups.iter_mut() {
            let Ok(event) = popup.events_rx.try_recv() else {
                continue;
            };
            match event {
                PopupLifecycleEvent::Ready(browser) => {
                    log::info!("tier3_pane::pane_host: popup delivered for pane={key}");
                    popup.lifecycle = PopupLifecycleState::Ready(browser);
                }
                PopupLifecycleEvent::Closed => closed_keys.push(key.clone()),
            }
        }

        let mut notifications = Vec::new();
        for key in closed_keys {
            self.popups.remove(&key);
            crate::tier3_pane::render::remove_popup_texture(&key);
            notifications.push(PopupNotification::Closed { key });
        }
        notifications
    }

    /// items.id=234: drains `PaneLoadHandler::on_load_start`'s navigate-away
    /// close requests (render.rs) -- see that handler's own doc for the
    /// parent-vs-popup disambiguation this relies on.
    fn drain_popup_close_requests(&mut self) -> Vec<PopupNotification> {
        let mut keys = Vec::new();
        while let Ok(key) = self.popup_close_rx.try_recv() {
            keys.push(key);
        }

        let mut notifications = Vec::new();
        for key in keys {
            if self.popups.contains_key(&key) {
                self.force_close_popup(&key);
                notifications.push(PopupNotification::Closed { key });
            }
        }
        notifications
    }
}

/// items.id=234: resolves a newly-requested popup's on-screen rect (as a
/// `PaneRectFraction` of the whole GLArea, matching every other pane-
/// geometry consumer's convention) from the size CEF's `PopupFeatures`
/// requested, if any, and the requesting pane's own current pixel rect, if
/// resolvable -- centering over it (items.id=234 plan, Judgment call 6.4:
/// backend-resolved and fixed at creation time, not frontend-measured,
/// since a `window.open()` call has no corresponding DOM node in QR's own
/// page to measure). `PopupFeatures`' width/height are treated as physical
/// pixels directly, not divided by any scale factor -- an accepted
/// simplification for this item's tunable, non-spec-derived default sizing,
/// not a precision guarantee. `features.x`/`features.y` are intentionally
/// unused: centering over the parent pane is this design's whole policy,
/// not honoring a page-requested position.
fn resolve_popup_rect(
    glarea_size: (u32, u32),
    parent_pixel_rect: Option<(u32, u32, u32, u32)>,
    features: &crate::tier3_pane::render::PopupFeatureInts,
) -> PaneRectFraction {
    const DEFAULT_WIDTH: u32 = 480;
    const DEFAULT_HEIGHT: u32 = 640;
    let (glarea_w, glarea_h) = (glarea_size.0.max(1), glarea_size.1.max(1));

    let width = features
        .width
        .filter(|w| *w > 0)
        .map(|w| w as u32)
        .unwrap_or(DEFAULT_WIDTH)
        .min(glarea_w);
    let height = features
        .height
        .filter(|h| *h > 0)
        .map(|h| h as u32)
        .unwrap_or(DEFAULT_HEIGHT)
        .min(glarea_h);

    let (center_x, center_y) = match parent_pixel_rect {
        Some((px, py, pw, ph)) => (px + pw / 2, py + ph / 2),
        None => (glarea_w / 2, glarea_h / 2),
    };
    let x = center_x.saturating_sub(width / 2).min(glarea_w - width);
    let y = center_y.saturating_sub(height / 2).min(glarea_h - height);

    PaneRectFraction {
        x: x as f64 / glarea_w as f64,
        y: y as f64 / glarea_h as f64,
        width: width as f64 / glarea_w as f64,
        height: height as f64 / glarea_h as f64,
    }
}

/// Converts a pane's `PaneRectFraction` (0..1 of the whole window's content
/// area) into a physical-pixel `(x, y, width, height)` rect within
/// `container_size`, clamped to stay inside it. `None` for a degenerate
/// (zero or negative) result. Shared between `sync_pane_sizes` above, the
/// `glarea`-level click/mouse hit-testing and input-shape routing in
/// `PaneHost::install` (items.id=257, Path A redesign) -- the single source
/// of truth this project's own pane-rect geometry requirement demands,
/// used by both rendering and hit-testing rather than two independently
/// -maintained copies -- and render.rs's own per-pane viewport computation:
/// small, intentional duplication of the same handful of lines rather than
/// a cross-module dependency between "what size should CEF think this pane
/// is" and "what rect should wgpu draw this pane's texture into," which are
/// related but separately-owned concerns (one drives CEF's layout, the
/// other drives compositing).
fn pane_pixel_rect(
    container_size: (u32, u32),
    frac: &PaneRectFraction,
) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (container_size.0 as f64, container_size.1 as f64);
    let x = (frac.x * w).round().clamp(0.0, w);
    let y = (frac.y * h).round().clamp(0.0, h);
    let width = (frac.width * w).round().clamp(0.0, w - x);
    let height = (frac.height * h).round().clamp(0.0, h - y);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((x as u32, y as u32, width as u32, height as u32))
}

/// Synthesizes the `mousedown` half of a mouse back/forward (button 8/9)
/// browser-navigation JS event, ported line-for-line from wry's own
/// `webkitgtk::synthetic_mouse_events::create_js_mouse_event` (private to
/// wry's crate, source at wry-0.55.1/src/webkitgtk/synthetic_mouse_events.rs)
/// -- see `PaneHost::install`'s doc comment for why this is reimplemented
/// here instead of reused. `held` is the BACK=0b01/FORWARD=0b10 bitmask
/// after this press is folded in, matching what wry's own `BackForwardState`
/// tracks; only the mousedown branch is needed since mouseup (and the real
/// `window.history.back()/forward()` trigger) still comes from wry's own
/// untouched button-release-event handler.
fn mouse_backforward_mousedown_js(event: &gtk::gdk::EventButton, held: u8) -> String {
    let button = if event.button() == 8 { 3 } else { 4 };
    let (x, y) = event.position();
    let (x, y) = (x as i32, y as i32);
    let modifiers_state = event.state();
    let mut buttons = 0;
    if modifiers_state.contains(gtk::gdk::ModifierType::BUTTON1_MASK) {
        buttons += 1;
    }
    if modifiers_state.contains(gtk::gdk::ModifierType::BUTTON3_MASK) {
        buttons += 2;
    }
    if modifiers_state.contains(gtk::gdk::ModifierType::BUTTON2_MASK) {
        buttons += 4;
    }
    if held & 0b01 != 0 {
        buttons += 8;
    }
    if held & 0b10 != 0 {
        buttons += 16;
    }
    format!(
        r#"(() => {{
        const el = document.elementFromPoint({x},{y});
        const ev = new MouseEvent('mousedown', {{
          view: window,
          button: {button},
          buttons: {buttons},
          x: {x},
          y: {y},
          bubbles: true,
          detail: {detail},
          cancelBubble: false,
          cancelable: true,
          clientX: {x},
          clientY: {y},
          composed: true,
          layerX: {x},
          layerY: {y},
          pageX: {x},
          pageY: {y},
          screenX: window.screenX + {x},
          screenY: window.screenY + {y},
          ctrlKey: {ctrl_key},
          metaKey: {meta_key},
          shiftKey: {shift_key},
          altKey: {alt_key},
        }});
        el.dispatchEvent(ev)
      }})()"#,
        x = x,
        y = y,
        detail = event.click_count().unwrap_or(1),
        ctrl_key = modifiers_state.contains(gtk::gdk::ModifierType::CONTROL_MASK),
        alt_key = modifiers_state.contains(gtk::gdk::ModifierType::MOD1_MASK),
        shift_key = modifiers_state.contains(gtk::gdk::ModifierType::SHIFT_MASK),
        meta_key = modifiers_state.contains(gtk::gdk::ModifierType::SUPER_MASK),
        button = button,
        buttons = buttons,
    )
}

// ---------------------------------------------------------------------------
// Pane content click/mouse forwarding (items.id=257)
// ---------------------------------------------------------------------------
//
// ACTIVE MECHANISM (Path B redesign, 2026-08-22): Path A -- `glarea` itself
// claiming pointer events via a GDK input shape carved out of its own
// private `event_window` -- is gone. Confirmed via live `gdb` this session:
// giving that shape a non-empty region froze the whole client's Wayland
// pointer input, root-caused to `event_window` being a non-native,
// client-side-only child `GdkWindow` under GTK3's CSW model -- Wayland's
// backend never gives non-native child windows a real compositor surface to
// route input through; only the toplevel has one. `glarea` no longer claims
// any pointer events at all; it is a pure compositor now, exactly as
// pass-through as if it weren't there for input purposes (see
// `connect_realize` below -- `event_window`'s input shape is initialized
// empty once at realize and never rebuilt afterward).
//
// Hit-testing moved into the frontend's own DOM instead: one invisible,
// precisely-positioned `<div>` per open pane (`PaneHitLayer`, computed from
// the exact same per-pane pixel rect the frontend already derives before
// dividing into the `PaneRectFraction` it sends via `set_pane_layout` -- see
// `paneLayout.ts`), receiving real native pointer events the browser's own
// hit-testing already scopes correctly -- no GDK-level scoping needed.
// Those events are forwarded over `forward_pane_mouse_click`/
// `forward_pane_mouse_move`/`forward_pane_mouse_wheel`
// (commands/tier3_pane.rs) to `PaneCommand::MouseClick`/`MouseMove`/
// `MouseWheel`, handled in `PaneHost::dispatch` below -- see
// `cef_modifiers_from_dom`'s own doc for why event coordinates arrive
// already pane-local, needing no origin-subtraction or scale-factor
// division the way the deleted GDK path's `cef_mouse_event` required.
//
// This supersedes the sibling-overlay-widget approach immediately below
// (`build_pane_hit_widget`), which is CONFIRMED BLOCKED, not merely
// deprioritized: `hit_widget` never once received a `button-press-event`,
// and its own diagnostic paint never rendered, despite correct positioning
// and `is_realized`/`is_mapped`/`has_window` all reporting `true` --
// suspected cause `glarea`'s raw-GL framebuffer write bypassing sibling
// Cairo compositing and/or `set_overlay_pass_through` routing input only to
// the overlay's main child. See `build_pane_hit_widget`'s own doc for the
// full reproduction; that function and its "KNOWN BLOCKER" documentation
// are kept in place, unused (`#[allow(dead_code)]`), as a preserved
// investigation artifact -- do not delete: its root cause was never
// identified, unlike Path A below, whose failure mode is fully documented
// in items.id=257's own tracking record and this file's git history, so
// deleting Path A's dead code outright adds no information those don't
// already preserve.

/// Translates GDK's modifier/button state bitmask into the bitmask CEF's
/// `MouseEvent.modifiers` expects (`cef::sys::cef_event_flags_t`'s bits --
/// `MouseEvent.modifiers` is a plain `u32`, not the higher-level `EventFlags`
/// wrapper type, so this builds the raw bitmask directly). Only the bits
/// CEF/Chromium actually reads for mouse routing are translated.
fn cef_modifiers_from_gdk(state: gtk::gdk::ModifierType) -> u32 {
    use cef::sys::cef_event_flags_t as Flag;
    let mut flags = Flag::EVENTFLAG_NONE;
    if state.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
        flags |= Flag::EVENTFLAG_SHIFT_DOWN;
    }
    if state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
        flags |= Flag::EVENTFLAG_CONTROL_DOWN;
    }
    if state.contains(gtk::gdk::ModifierType::MOD1_MASK) {
        flags |= Flag::EVENTFLAG_ALT_DOWN;
    }
    if state.contains(gtk::gdk::ModifierType::SUPER_MASK) {
        flags |= Flag::EVENTFLAG_COMMAND_DOWN;
    }
    if state.contains(gtk::gdk::ModifierType::BUTTON1_MASK) {
        flags |= Flag::EVENTFLAG_LEFT_MOUSE_BUTTON;
    }
    if state.contains(gtk::gdk::ModifierType::BUTTON2_MASK) {
        flags |= Flag::EVENTFLAG_MIDDLE_MOUSE_BUTTON;
    }
    if state.contains(gtk::gdk::ModifierType::BUTTON3_MASK) {
        flags |= Flag::EVENTFLAG_RIGHT_MOUSE_BUTTON;
    }
    flags.0
}

/// GDK's button number -> CEF's 3-button model. GDK also reports 8/9
/// (back/forward), which have no `MouseButtonType` equivalent in CEF and are
/// not forwarded -- back/forward-in-pane-content is not part of this scope
/// (contrast `PaneHost::install`'s own back/forward reimplementation, which
/// operates on QR's own webview widget, not pane content, for items.id=227).
fn cef_mouse_button_from_gdk(button: u32) -> Option<MouseButtonType> {
    match button {
        1 => Some(MouseButtonType::LEFT),
        2 => Some(MouseButtonType::MIDDLE),
        3 => Some(MouseButtonType::RIGHT),
        _ => None,
    }
}

/// DOM-sourced counterpart to `cef_modifiers_from_gdk`, for the Path B
/// click-routing mechanism (items.id=257, see this section's own doc
/// above). A browser `PointerEvent`/`WheelEvent` reports its modifier keys
/// as four separate booleans (`shiftKey`/`ctrlKey`/`altKey`/`metaKey`)
/// rather than GDK's single bitmask, and `buttons` as its own bitmask with a
/// *different* bit order than GDK's (`MouseEvent.buttons`: bit0=left,
/// bit1=right, bit2=middle -- see MDN) -- not merged into one function
/// taking some shared intermediate type, since the two input sources have
/// no natural common representation and forcing one would just relocate the
/// conversion rather than remove it. Same `cef::sys::cef_event_flags_t` bits
/// as the GDK version.
fn cef_modifiers_from_dom(shift: bool, ctrl: bool, alt: bool, meta: bool, buttons: u16) -> u32 {
    use cef::sys::cef_event_flags_t as Flag;
    let mut flags = Flag::EVENTFLAG_NONE;
    if shift {
        flags |= Flag::EVENTFLAG_SHIFT_DOWN;
    }
    if ctrl {
        flags |= Flag::EVENTFLAG_CONTROL_DOWN;
    }
    if alt {
        flags |= Flag::EVENTFLAG_ALT_DOWN;
    }
    if meta {
        flags |= Flag::EVENTFLAG_COMMAND_DOWN;
    }
    if buttons & 0b001 != 0 {
        flags |= Flag::EVENTFLAG_LEFT_MOUSE_BUTTON;
    }
    if buttons & 0b010 != 0 {
        flags |= Flag::EVENTFLAG_RIGHT_MOUSE_BUTTON;
    }
    if buttons & 0b100 != 0 {
        flags |= Flag::EVENTFLAG_MIDDLE_MOUSE_BUTTON;
    }
    flags.0
}

/// KNOWN BLOCKER, confirmed by hand this session, not yet root-caused:
/// clicks never reach this widget's `button-press-event`, and this widget's
/// own `connect_draw` paint (kept in place below as a diagnostic, not
/// cosmetic) never becomes visible on screen -- despite `get_child_position`
/// (in `install()`) resolving a correct, verified-by-log rect for it every
/// time, and `hit_widget.{is_realized,is_mapped,has_window}()` all reporting
/// `true` immediately after `show()`. Both symptoms reproduce with a
/// full-window (0,0,1,1) rect, ruling out a positioning-math bug. Suspected
/// cause: `glarea` bypasses GTK's normal Cairo compositing entirely, writing
/// directly into GTK's shared native framebuffer every render tick (see
/// `RenderState::render`'s own doc, "there is no `wgpu::Surface` here...
/// GTK already owns presentation") -- plausibly overwriting whatever this
/// sibling overlay child's ordinary Cairo `draw` composited, and/or
/// `overlay.set_overlay_pass_through(&glarea, true)` (items.id=225/227)
/// routes input straight to the overlay's MAIN child (the webview),
/// bypassing OTHER overlay children like this one entirely rather than
/// falling through to "whatever's next in the stack." Neither theory is
/// confirmed against GTK3's actual C source or a WAYLAND_DEBUG capture --
/// that's the next step, not guessed further here.
///
/// A more promising redesign, grounded in this project's OWN prior
/// confirmed finding rather than a new assumption: items.id=227's own
/// investigation (see the "ROOT CAUSE FOUND" comment in `connect_realize`
/// below) explicitly confirmed `glarea`'s *own* `button-press-event` fires
/// reliably on every real click ("confirmed via a now-removed temporary
/// widget-level trace"). Routing input handling through `glarea`'s own
/// event signals directly -- hit-testing each open pane's rect manually
/// inside that handler, rather than via a separate sibling overlay widget
/// per pane -- avoids this entire class of problem, at the cost of needing
/// a manual re-forward (mirroring `mouse_backforward_mousedown_js` below)
/// for clicks that land outside every open pane's rect, since claiming the
/// event on `glarea` itself means `overlay.set_overlay_pass_through` can no
/// longer do that forwarding for free.
///
/// Builds one pane's click-catching overlay widget and wires its pointer
/// signals to forward into that pane's own CEF browser via the same
/// `pane.browser_lifecycle.browser().and_then(|b| b.host())` accessor
/// `sync_pane_sizes`/`close_pane` already use for `was_resized()`/
/// `close_browser()`. Paints nothing -- purely an input target sitting in
/// front of the shared `glarea`'s composited CEF texture, which remains what
/// the user actually sees (this widget and the glarea are separate overlay
/// children of the same `gtk::Overlay`; this one only ever intercepts
/// events, never pixels).
///
/// Coordinates: `event.position()` is already local to this widget's own
/// allocation (0,0 at its own top-left) since GTK delivers widget-relative
/// coordinates -- no manual pane-origin subtraction is needed. Scaling by
/// `glarea.scale_factor()` before handing to CEF matches the exact
/// convention `sync_pane_sizes` already uses to compute this pane's
/// `LogicalSize` (`pane_host.rs`'s own physical-pixels -> CEF-logical-pixels
/// division), so this pane's `PaneRenderHandler::view_rect` and these
/// forwarded coordinates agree on the same coordinate space.
///
/// SUPERSEDED (items.id=257, Path A redesign, 2026-08-22) -- kept in place,
/// unused, as a preserved investigation artifact per this project's own
/// forensic-trail discipline; not called from `PaneHost::dispatch` anymore.
/// See the section doc above for the active mechanism.
#[allow(dead_code)]
fn build_pane_hit_widget(
    key: PaneKey,
    manager: Rc<RefCell<PaneManager>>,
    glarea: gtk::GLArea,
) -> gtk::DrawingArea {
    let hit_widget = gtk::DrawingArea::new();
    hit_widget.set_can_focus(true);
    // Default packing GTK falls back to when `get_child_position` (installed
    // in `install()`) returns `None` for this widget -- which it does until
    // `PaneLayoutState` actually has an entry for this pane (a real gap: a
    // pane opens and its browser starts loading before the frontend's first
    // `set_pane_layout` call lands). Without this, an unconfigured
    // `DrawingArea`'s default alignment (`Fill`/`Fill`) makes GTK's overlay
    // packing give it the WHOLE overlay's allocation in that window --
    // confirmed by hand this session: an opened-but-not-yet-laid-out pane's
    // hit_widget silently ate every click and keyboard-focus grab across the
    // entire app window, including panels unrelated to any pane, until the
    // pane closed. Pinning it to a zero-size widget anchored at the origin
    // means "not yet positioned" is inert instead of "covers everything."
    hit_widget.set_halign(gtk::Align::Start);
    hit_widget.set_valign(gtk::Align::Start);
    hit_widget.set_size_request(0, 0);
    hit_widget.add_events(
        gtk::gdk::EventMask::BUTTON_PRESS_MASK
            | gtk::gdk::EventMask::BUTTON_RELEASE_MASK
            | gtk::gdk::EventMask::POINTER_MOTION_MASK
            | gtk::gdk::EventMask::LEAVE_NOTIFY_MASK
            | gtk::gdk::EventMask::SCROLL_MASK,
    );
    // DIAG items.id=257: NOT cosmetic -- this is load-bearing evidence for
    // the open blocker documented on `build_pane_hit_widget` above. Confirmed
    // by hand this session: this handler fires every frame (hundreds of
    // times over one manual test), yet the red tint it paints is never once
    // visible on screen, and this widget's button-press-event never fires
    // for a real click landing squarely inside its own confirmed-correct
    // `get_child_position` rect either. Left in place -- along with the RAW
    // button_press_event log below -- as the reproduction case for whoever
    // picks up the investigation this doc points at. Do not delete without
    // first re-confirming the blocker is actually resolved.
    hit_widget.connect_draw(|_widget, cr| {
        log::info!("DIAG items.id=257: hit_widget connect_draw FIRED");
        cr.set_source_rgba(1.0, 0.0, 0.0, 0.25);
        let _ = cr.paint();
        glib::Propagation::Proceed
    });

    fn mouse_event(glarea: &gtk::GLArea, x: f64, y: f64, state: gtk::gdk::ModifierType) -> MouseEvent {
        let scale = glarea.scale_factor().max(1) as f32;
        MouseEvent {
            x: (x as f32 / scale) as i32,
            y: (y as f32 / scale) as i32,
            modifiers: cef_modifiers_from_gdk(state),
        }
    }

    {
        let manager = manager.clone();
        let glarea = glarea.clone();
        let key = key.clone();
        hit_widget.connect_button_press_event(move |widget, event| {
            // Click-to-focus, mirroring ordinary desktop pane/window
            // behavior -- harmless even though nothing reads
            // `PaneManager::focused_pane` yet this session (keyboard
            // forwarding is a separate follow-on).
            widget.grab_focus();
            log::info!(
                "DIAG items.id=257: RAW button_press_event fired, button={} pos={:?}",
                event.button(), event.position()
            );
            let Some(button) = cef_mouse_button_from_gdk(event.button()) else {
                return glib::Propagation::Proceed;
            };
            let (x, y) = event.position();
            let ev = mouse_event(&glarea, x, y, event.state());
            let mut mgr = manager.borrow_mut();
            mgr.focused_pane = Some(key.clone());
            log::info!(
                "DIAG items.id=257: button_press pane={key} widget_local=({x},{y}) cef=({},{}) has_host={}",
                ev.x, ev.y,
                mgr.panes.get(&key).and_then(|p| p.browser_lifecycle.browser()).and_then(|b| b.host()).is_some()
            );
            if let Some(host) = mgr
                .panes
                .get(&key)
                .and_then(|p| p.browser_lifecycle.browser())
                .and_then(|b| b.host())
            {
                host.send_mouse_click_event(
                    Some(&ev),
                    button,
                    false as _,
                    event.click_count().unwrap_or(1) as _,
                );
            }
            glib::Propagation::Stop
        });
    }

    {
        let manager = manager.clone();
        let glarea = glarea.clone();
        let key = key.clone();
        hit_widget.connect_button_release_event(move |_widget, event| {
            let Some(button) = cef_mouse_button_from_gdk(event.button()) else {
                return glib::Propagation::Proceed;
            };
            let (x, y) = event.position();
            let ev = mouse_event(&glarea, x, y, event.state());
            if let Some(host) = manager
                .borrow()
                .panes
                .get(&key)
                .and_then(|p| p.browser_lifecycle.browser())
                .and_then(|b| b.host())
            {
                host.send_mouse_click_event(
                    Some(&ev),
                    button,
                    true as _,
                    event.click_count().unwrap_or(1) as _,
                );
            }
            glib::Propagation::Stop
        });
    }

    {
        let manager = manager.clone();
        let glarea = glarea.clone();
        let key = key.clone();
        hit_widget.connect_motion_notify_event(move |_widget, event| {
            let (x, y) = event.position();
            let ev = mouse_event(&glarea, x, y, event.state());
            if let Some(host) = manager
                .borrow()
                .panes
                .get(&key)
                .and_then(|p| p.browser_lifecycle.browser())
                .and_then(|b| b.host())
            {
                host.send_mouse_move_event(Some(&ev), false as _);
            }
            glib::Propagation::Proceed
        });
    }

    {
        let manager = manager.clone();
        let glarea = glarea.clone();
        let key = key.clone();
        hit_widget.connect_leave_notify_event(move |_widget, event| {
            let (x, y) = event.position();
            let ev = mouse_event(&glarea, x, y, event.state());
            if let Some(host) = manager
                .borrow()
                .panes
                .get(&key)
                .and_then(|p| p.browser_lifecycle.browser())
                .and_then(|b| b.host())
            {
                host.send_mouse_move_event(Some(&ev), true as _);
            }
            glib::Propagation::Proceed
        });
    }

    {
        let manager = manager.clone();
        let glarea = glarea.clone();
        let key = key.clone();
        // Sign/scale not manually verified against a real scroll gesture
        // this session (click forwarding was this session's actual scope --
        // wheel support is included since the plan named it, but treat this
        // one as unverified). CEF/Chromium's convention (positive delta_y ==
        // content scrolls up) is assumed to match GTK's own delta sign
        // as-is; flip/rescale here if manual testing shows it's inverted or
        // mis-scaled.
        hit_widget.connect_scroll_event(move |_widget, event| {
            const PIXELS_PER_SCROLL_UNIT: f64 = 40.0;
            let (dx, dy) = match event.direction() {
                gtk::gdk::ScrollDirection::Up => (0.0, 1.0),
                gtk::gdk::ScrollDirection::Down => (0.0, -1.0),
                gtk::gdk::ScrollDirection::Left => (1.0, 0.0),
                gtk::gdk::ScrollDirection::Right => (-1.0, 0.0),
                _ => event.delta(),
            };
            let (x, y) = event.position();
            let ev = mouse_event(&glarea, x, y, event.state());
            if let Some(host) = manager
                .borrow()
                .panes
                .get(&key)
                .and_then(|p| p.browser_lifecycle.browser())
                .and_then(|b| b.host())
            {
                host.send_mouse_wheel_event(
                    Some(&ev),
                    (dx * PIXELS_PER_SCROLL_UNIT) as i32,
                    (dy * PIXELS_PER_SCROLL_UNIT) as i32,
                );
            }
            glib::Propagation::Stop
        });
    }

    hit_widget
}

/// Owns the single shared `gtk::GLArea`, the single shared `RenderState`,
/// and every currently-open pane's state -- the single-window replacement
/// for the old `PaneWindow` facade. Not `Send` (GTK objects aren't, same as
/// winit's X11 IME pointers weren't) -- kept off `tauri::State`, owned
/// directly by `app.run()`'s closure in main.rs, same pattern as before.
pub struct PaneHost {
    glarea: gtk::GLArea,
    render_state: Rc<RefCell<Option<RenderState>>>,
    manager: Rc<RefCell<PaneManager>>,
    open_pane_count: Arc<AtomicUsize>,
}

impl PaneHost {
    /// Reparents Tauri's own webview widget into a `gtk::Overlay` and adds
    /// the shared `GLArea` as a transparent overlay child above it, then
    /// wires the GLArea's `realize`/`resize`/`render` signals. Must run on
    /// the main thread, after the Tauri window has been built (needs
    /// `WebviewWindow::gtk_window()`/`default_vbox()`, both real APIs but
    /// only valid once the window exists).
    ///
    /// The riskiest step in this whole rewrite: reparenting wry's
    /// already-constructed webview widget (not a bespoke throwaway GTK app,
    /// which is what this session's own spike validated) into a new
    /// `gtk::Overlay` inside the real Tauri app. wry may hold invariants
    /// tied to the widget's original parent that this doesn't currently
    /// know about -- flagged explicitly, not glossed over, per this
    /// project's own review-before-trusting-a-first-attempt discipline.
    /// Manual verification (see the harness) must confirm the webview is
    /// still fully interactive after this, not just that it still paints.
    pub fn install(window: &tauri::WebviewWindow, app_handle: tauri::AppHandle) -> Self {
        let vbox = window
            .default_vbox()
            .expect("tier3_pane::pane_host: could not get main window's default_vbox");

        let webview_widget =
            vbox.children().into_iter().next().expect(
                "tier3_pane::pane_host: main window's default_vbox has no child to overlay",
            );
        vbox.remove(&webview_widget);

        // FIX (items.id=227, 2026-08-08): tauri-runtime-wry installs a
        // button-press-event AND a touch-event handler directly on this
        // webview widget during webview creation itself (tauri-runtime-
        // wry-2.11.2/src/lib.rs:5277, unconditional on Linux -- the only
        // decorated/resizable check is inside the handler, after the
        // crash below). Both walk `webview.parent().and_then(|w|
        // w.parent())` expecting stock tao/wry's fixed two-level webview
        // -> GtkBox -> GtkWindow layout, then
        // `.downcast::<gtk::Window>().unwrap()` -- their own comment says
        // "Safe to unwrap unless this is not from tao". Reparenting the
        // webview one level deeper into our own gtk::Overlay below makes
        // exactly that not-from-tao case real: the "grandparent" becomes
        // our vbox (a GtkBox, not a GtkWindow), the downcast returns Err,
        // and the unwrap panics inside a GTK signal callback -- which
        // can't unwind across the C FFI boundary, so the whole process
        // aborts. This was never triggered before because the GLArea
        // pass-through fix above is what first let a real click reach
        // this widget at all; fixing that bug is what surfaced this one.
        // This app never uses undecorated/borderless windows
        // (tauri.conf.json has no `decorations` override), so the
        // resize-drag feature these handlers exist for is dead weight
        // here even when it doesn't crash.
        //
        // Confirmed independently (WebKit's own source,
        // WebKitWebViewBase.cpp:2454 -- `widgetClass->button_press_event
        // = webkitWebViewBaseButtonPressEvent`) that WebKitGTK's actual
        // page/DOM click delivery is wired through the GtkWidgetClass
        // vfunc slot at class-init time, not a g_signal_connect() closure
        // -- disconnecting externally-connected handlers below cannot
        // reach or affect it.
        //
        // Neither gtk-rs nor tauri-runtime-wry ever hands back the
        // SignalHandlerId for either handler (both connected internally,
        // opaque to our code), so the only way to remove them is
        // g_signal_handlers_disconnect_matched() matched by signal alone.
        // touch-event has no other consumer sharing it, so it comes off
        // clean. button-press-event does NOT: wry's own (not tauri-
        // runtime-wry's) synthetic_mouse_events.rs shares that exact
        // signal on this exact widget for mouse button 8/9 (back/forward)
        // navigation, and a signal-only match can't distinguish the two
        // internal handlers from each other (no exported symbol to tell
        // gtk-rs's generic per-closure trampolines apart, and that module
        // is private to wry's own crate, so we cannot just call its
        // setup() again afterward). Reimplemented immediately below
        // instead, as code we own outright: a fresh closure that shares
        // no code path, and in particular never touches window ancestry,
        // with undecorated_resizing.rs's crashing handler.
        //
        // External review caught a real gap here (2026-08-09): a bare
        // signal-only match is a promise about how many handlers exist
        // *right now*, on this exact wry/tauri-runtime-wry version -- not
        // a guarantee that stays true. A future WebKitGTK version, a
        // different Tauri plugin, or anything else that ever connects to
        // button-press-event/touch-event on this same widget would be
        // swept up here too, silently, with no signal anything changed.
        // Each disconnect call's own return value (the count actually
        // disconnected) is checked against what this comment claims above
        // -- 2 for button-press-event, 1 for touch-event -- and logged at
        // error level, loud enough to notice, if that ever drifts. Not a
        // hard panic/assert: a version bump silently adding a THIRD
        // legitimate handler here shouldn't crash the whole app on
        // startup, but it must be impossible to miss in the logs.
        {
            use gtk::glib::{gobject_ffi, translate::IntoGlib};
            let obj = webview_widget.upcast_ref::<gtk::glib::Object>();
            let widget_gtype = obj.type_().into_glib();
            for (signal_name, expected_count) in
                [(c"button-press-event", 2u32), (c"touch-event", 1u32)]
            {
                // SAFETY:
                // - obj.as_ptr() is valid and the referenced GObject is
                //   alive for this entire call: `webview_widget` (and
                //   `obj`, an upcast reference to it) is an owned,
                //   reference-counted GTK widget held on this stack frame
                //   for the whole unsafe block -- it cannot be dropped or
                //   finalized out from under these calls.
                // - `signal_id` is guaranteed to belong to (or be
                //   inherited by) `obj`'s own type: it comes from
                //   g_signal_lookup(name, widget_gtype), and widget_gtype
                //   is obj.type_() -- this object's own runtime type, not
                //   a different/unrelated one.
                // - Both calls happen on the correct thread for GObject/
                //   GTK signal APIs (never thread-safe to call off the
                //   thread that owns the main loop): this code runs
                //   inside PaneHost::install, called from main.rs's
                //   app.run(...) closure on tauri::RunEvent::Ready --
                //   Tauri/tao's own main event-loop callback, which IS
                //   the GTK main thread on this Linux backend.
                // - No raw pointer obtained here escapes this block: the
                //   `*mut GObject` from obj.as_ptr() is used only as an
                //   argument to these two FFI calls below, never stored,
                //   returned, or captured into anything longer-lived.
                unsafe {
                    let signal_id =
                        gobject_ffi::g_signal_lookup(signal_name.as_ptr(), widget_gtype);
                    if signal_id == 0 {
                        log::warn!(
                            "tier3_pane::pane_host: g_signal_lookup found no {signal_name:?} \
                             signal on the webview widget's type -- nothing disconnected"
                        );
                        continue;
                    }
                    let disconnected = gobject_ffi::g_signal_handlers_disconnect_matched(
                        obj.as_ptr(),
                        gobject_ffi::G_SIGNAL_MATCH_ID,
                        signal_id,
                        0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    if disconnected == expected_count {
                        log::info!(
                            "tier3_pane::pane_host: disconnected {disconnected} \
                             {signal_name:?} handler(s) from the webview widget as \
                             expected (removing tauri-runtime-wry's undecorated-resize \
                             crash path -- items.id=227)"
                        );
                    } else {
                        log::error!(
                            "tier3_pane::pane_host: EXPECTED COUNT MISMATCH disconnecting \
                             {signal_name:?} from the webview widget -- removed \
                             {disconnected}, expected {expected_count}. items.id=227's \
                             blanket disconnect assumed exactly the handlers known at the \
                             time this was written (tauri-runtime-wry 2.11.2 / wry 0.55.1) \
                             -- a different count means either a version bump changed \
                             what's connected here, or a new consumer (another plugin?) \
                             now shares this signal too, and it was just silently \
                             disconnected along with the rest. Investigate before trusting \
                             mouse/touch input on this widget."
                        );
                    }
                }
            }
        }
        // Reimplementation of wry's synthetic_mouse_events.rs mousedown
        // half (button 8/9 back/forward -> synthesized JS `mousedown`),
        // ported from that module's actual source rather than guessed --
        // the mouseup half (and the real window.history.back()/forward()
        // trigger, which lives in ITS js string's mouseup branch) is
        // untouched, still wry's own original button-release-event
        // handler, since that signal was never disconnected above. This
        // half's own held-button state (`press_state`) is intentionally a
        // fresh, independent Rc from wry's own -- it does not observe
        // what the surviving release handler's state does or vice versa.
        // The only place that would matter is the `buttons` bitmask on a
        // MouseEvent if back AND forward were both already held when this
        // fires, which is not a real usage pattern for a back/forward
        // side-button click; accepted as-is rather than reaching into
        // wry's private state to unify it.
        if let Ok(webview) = webview_widget.clone().downcast::<webkit2gtk::WebView>() {
            use webkit2gtk::WebViewExt;
            webview_widget.add_events(
                gtk::gdk::EventMask::BUTTON1_MOTION_MASK | gtk::gdk::EventMask::BUTTON_PRESS_MASK,
            );
            let press_state: Rc<RefCell<u8>> = Rc::new(RefCell::new(0));
            webview_widget.connect_button_press_event(
                move |_widget, event: &gtk::gdk::EventButton| match event.button() {
                    8 | 9 => {
                        let held = {
                            let mut state = press_state.borrow_mut();
                            *state |= if event.button() == 8 { 0b01 } else { 0b10 };
                            *state
                        };
                        webview.evaluate_javascript(
                            &mouse_backforward_mousedown_js(event, held),
                            None,
                            None,
                            None::<&gtk::gio::Cancellable>,
                            |_| {},
                        );
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                },
            );
        } else {
            log::warn!(
                "tier3_pane::pane_host: main window's webview widget is not a \
                 webkit2gtk::WebView -- items.id=227's mouse back/forward \
                 reimplementation was not installed"
            );
        }

        let overlay = gtk::Overlay::new();
        overlay.add(&webview_widget);

        let glarea = gtk::GLArea::new();
        glarea.set_has_alpha(true);
        glarea.set_hexpand(true);
        glarea.set_vexpand(true);
        // DIAGNOSTIC (items.id=225, 2026-08-07): the GLArea overlay appears
        // to swallow all pointer input across the whole window even with
        // overlay pass-through set below -- confirmed via direct click
        // testing (Tier3Selector checkboxes and the harness's own
        // "Simulate response generating" button are both completely inert).
        // set_can_focus(false) rules out one candidate cause (the GLArea
        // grabbing keyboard/click-to-focus before pass-through routing).
        glarea.set_can_focus(false);
        // Redraw only on an explicit queue_draw() call (from the GLib
        // timeout below, gated on open_pane_count > 0) -- not on every GTK
        // frame-clock tick regardless of whether any pane is open. Matches
        // items.id=223's on-demand-cost discipline: a user who never opens
        // Tier 2/3 should not pay for a continuously re-rendering GLArea.
        glarea.set_auto_render(false);

        overlay.add_overlay(&glarea);
        // Input forwarding into CEF does not exist yet at any layer (a
        // real, pre-existing gap -- see tier3_pane/mod.rs docs) -- until it
        // does, mouse/keyboard events must keep reaching the webview
        // underneath, not be swallowed by an overlay child that can't yet
        // do anything with them.
        overlay.set_overlay_pass_through(&glarea, true);

        // ROOT CAUSE FOUND (items.id=225, 2026-08-07): every signal handler
        // below (`connect_realize`/`connect_resize`/`connect_render`) was
        // previously connected AFTER `vbox.add(&overlay); overlay.show_all();`
        // -- confirmed via diagnostic logging that `overlay.show_all()`
        // itself synchronously realizes the whole subtree (GtkOverlay/
        // GtkGLArea/the reparented webview all report realized=true
        // immediately after it returns). GTK does not replay a signal for
        // handlers connected after it already fired, so `connect_realize`'s
        // body -- the ONLY place `RenderState`/`gl_context` ever get
        // constructed -- has never once executed in the live app (confirmed:
        // its own log lines never appeared in any run). All signal
        // connections now happen BEFORE `show_all()`, matching ordinary GTK
        // usage (connect signals, then show).
        let render_state: Rc<RefCell<Option<RenderState>>> = Rc::new(RefCell::new(None));
        // Separate from RenderState's own internal glow::Context (built
        // inside wgpu_hal::gles::Adapter::new_external, never exposed back
        // out) -- this one is only for the raw GL_DRAW_FRAMEBUFFER_BINDING
        // capture/rebind calls single-window compositing needs outside
        // wgpu's own abstraction (see the `render` handler below and
        // gl_loader.rs's module doc).
        let gl_context: Rc<RefCell<Option<glow::Context>>> = Rc::new(RefCell::new(None));
        // ROOT CAUSE FOUND (items.id=226, 2026-08-08): confirmed via gdb
        // against a real coredump that the previous code opened a
        // `GlProcLoader` as a closure-local inside `connect_realize`, fed
        // its `loader_fn()` to both `RenderState::new` and
        // `glow::Context::from_loader_function`, then let it drop (and
        // `dlclose` its `libGLESv2.so.2`/`libEGL.so.1` handles) at the end
        // of that same closure invocation. Both consumers cache raw
        // resolved function pointers at construction time and never touch
        // the loader again -- once its library handles were closed and
        // (confirmed: `libGLESv2.so.2` was completely absent from the
        // crashed process's own memory map, unlike `libGLdispatch.so.0`/
        // `libEGL.so.1`, which have other independent referrers) actually
        // unmapped, every one of those cached pointers was dangling. The
        // very first real GL call issued each frame (the
        // `GL_DRAW_FRAMEBUFFER_BINDING` capture below) jumped through one
        // and segfaulted. Fix: one `GlProcLoader`, opened once, kept alive
        // in this `Rc` for the GLArea's whole realized lifetime -- matching
        // gl_loader.rs's own already-documented contract ("callers keep
        // this alive for as long as they need proc-address resolution"),
        // which the old call site simply didn't follow. Both consumers now
        // share this one instance instead of each opening (and dropping)
        // their own -- gl_loader.rs's per-call-site-opens-its-own-instance
        // guidance predates this bug and is superseded by it.
        //
        // `RefCell<Option<_>>`, not an eagerly-constructed `GlProcLoader`,
        // for the same reason `render_state`/`gl_context` above are: opening
        // it is only valid once the GLArea's context is current
        // (`area.make_current()`, called first thing in `connect_realize`
        // below) -- it cannot be constructed before the GLArea is realized.
        let gl_loader: Rc<RefCell<Option<crate::tier3_pane::gl_loader::GlProcLoader>>> =
            Rc::new(RefCell::new(None));
        // items.id=234: both channel pairs are created once, here, and
        // their sender halves cloned into each pane's PaneLifeSpanHandler/
        // PaneLoadHandler at open_pane time -- the receiver halves live on
        // PaneManager itself, drained once per GTK render tick.
        let (popup_requested_tx, popup_requested_rx) = std::sync::mpsc::channel();
        let (popup_close_tx, popup_close_rx) = std::sync::mpsc::channel();
        let manager = Rc::new(RefCell::new(PaneManager {
            panes: IndexMap::new(),
            open_pane_count: Arc::new(AtomicUsize::new(0)),
            focused_pane: None,
            popups: HashMap::new(),
            popup_requested_tx,
            popup_requested_rx,
            popup_close_tx,
            popup_close_rx,
            active_pane: None,
            app_handle: app_handle.clone(),
        }));
        let open_pane_count = manager.borrow().open_pane_count.clone();
        {
            let render_state = render_state.clone();
            let gl_context = gl_context.clone();
            let gl_loader = gl_loader.clone();
            let glarea_for_realize = glarea.clone();
            glarea.connect_realize(move |area| {
                area.make_current();
                if let Some(err) = area.error() {
                    log::error!("tier3_pane::pane_host: GLArea realize error: {err}");
                    return;
                }
                // ROOT CAUSE FOUND (items.id=227, 2026-08-08, confirmed against
                // GTK 3.24's actual C source, not just black-box testing):
                // items.id=225's reassertion below (window.set_pass_through)
                // was never wrong, it was just aimed at the wrong window.
                // GtkGLArea sets has_window=FALSE (gtk_gl_area_init) -- for a
                // no-window widget, area.window() resolves to its
                // *parent_window*, which GtkOverlay explicitly points at a
                // dedicated per-overlay-child GdkWindow it creates for us
                // (gtk_overlay_create_child_window) and *already* pass-throughs
                // correctly on our behalf (that's what
                // overlay.set_overlay_pass_through() actually does under the
                // hood). Confirmed via live click capture: both pass-through
                // calls report is_pass_through=true and are telling the
                // truth -- for that window. But gtk_gl_area_realize() (see
                // gtk/gtkglarea.c upstream) *unconditionally* creates a
                // second, private GDK_INPUT_ONLY window of its own
                // (priv->event_window, sized to the widget's own allocation,
                // parented one level *inside* the window pass-through was set
                // on) specifically to catch input for this has_window=FALSE
                // widget -- gtk_widget_register_window() ties it back to the
                // GLArea widget for signal dispatch, which is exactly why
                // GLArea's own button-press-event fired on every real click
                // during this investigation (confirmed via a now-removed
                // temporary widget-level trace) while the webview's never
                // did. This private window has no public accessor
                // anywhere in GTK3's API (gtk_gl_area_*, gtk_overlay_*, no
                // getter) and pass-through is a per-window flag, not
                // inherited by descendants -- so nothing reachable from
                // application code had ever touched it; it silently keeps
                // its GTK default of FALSE regardless of what we do to its
                // parent. Real fix: find it anyway via the one public GDK
                // API that can see it (gdk_window_get_children() on the
                // window pass-through already worked on) and set
                // pass-through on it directly. (items.id=257 Path A,
                // 2026-08-22, refines this further: instead of a blanket
                // pass-through -- which would make glarea and its panes
                // unable to receive input at all -- event_window's GDK
                // input SHAPE is restricted below to just the open panes'
                // rects, rebuilt as panes open/close/resize; see the
                // click/mouse-forwarding section doc above
                // `build_pane_hit_widget`.) gtk_gl_area_realize()
                // (upstream) only ever creates the one INPUT_ONLY child,
                // so today that means exactly one match -- but the code
                // below verifies that rather than assuming it (external
                // review, 2026-08-09): collect every INPUT_ONLY child and
                // match on the count, so a future GTK version creating
                // more than one can't get silently mis-handled the same
                // way the original wry ancestry assumption that started
                // this whole item was -- an unverified "there's only one
                // of these" is exactly the class of bug items.id=227 has
                // been chasing all along.
                //
                // connect_realize only fires once per realization, so this
                // fix only re-applies if GTK ever fires realize again.
                // Confirmed directly (not just asserted) that this app's
                // lifecycle never does that in practice: install() itself
                // only ever runs once, gated on tauri::RunEvent::Ready
                // (fires once per app lifetime -- see main.rs's app.run()
                // closure, the only call site), and nothing else in this
                // codebase hides, removes, or reparents the GLArea
                // afterward. GTK3 doesn't unrealize child widgets on
                // iconify/minimize either -- only on actual removal from a
                // realized parent, which never happens here post-install.
                //
                // Walked through what WOULD happen if a second realize
                // ever did occur, rather than just trusting "it's inside
                // connect_realize so it must be fine": gtk_gl_area_realize
                // (the class handler, runs before this closure on every
                // firing -- see GTK_WIDGET_CLASS(...)->realize(widget) at
                // the top of gtk_gl_area_realize upstream) unconditionally
                // creates a brand new priv->event_window every time it
                // runs, unrealize destroys the old one first. Nothing in
                // this closure caches the old event_window across calls --
                // `window.children()` below is a live GDK query issued
                // fresh every time this closure fires, so on a
                // hypothetical second realize it would enumerate whatever
                // INPUT_ONLY children exist at that moment (the new
                // event_window, not a stale reference to the destroyed
                // one) and correctly pass-through it again. area.window()
                // itself (the Overlay's own per-child window, not GLArea's
                // private one) is also queried fresh each call, not read
                // from a captured variable -- so even in the unlikely case
                // the Overlay recreated that window too, this would still
                // resolve correctly. This fix is structurally correct for
                // a second realize even though one never actually happens.
                if let Some(window) = area.window() {
                    window.set_pass_through(true);
                    let input_only_children: Vec<gtk::gdk::Window> = window
                        .children()
                        .into_iter()
                        .filter(|child| child.is_input_only())
                        .collect();
                    match input_only_children.as_slice() {
                        [event_window] => {
                            // pass_through(false) (GTK's own default -- set
                            // explicitly rather than left implicit, matching
                            // this codebase's defensive style elsewhere) is
                            // moot in practice now: this input shape is set
                            // empty here once and never rebuilt afterward
                            // (items.id=257 Path B -- pane click routing no
                            // longer goes through GDK at all, see the "Pane
                            // content click/mouse forwarding" section doc
                            // above), so there is no non-empty shape for
                            // pass-through to ever apply to. Kept explicit,
                            // and kept as exactly this one confirmed-safe
                            // `input_shape_combine_region` call (this
                            // session's own `gdb` work confirmed an EMPTY
                            // region here does not freeze Wayland pointer
                            // input; only a non-empty one did) rather than
                            // removed, since a permanently-empty shape is the
                            // smallest change that provably avoids the freeze.
                            event_window.set_pass_through(false);
                            event_window
                                .input_shape_combine_region(&gtk::cairo::Region::create(), 0, 0);
                            log::info!(
                                "tier3_pane::pane_host: GLArea private event_window \
                                 found, input shape set permanently empty (items.id=257 \
                                 Path B -- glarea is a pure compositor now, pane click \
                                 routing goes through the frontend's DOM hit-layer \
                                 instead), is_pass_through={} \
                                 (parent GdkWindow is_pass_through={})",
                                event_window.is_pass_through(),
                                window.is_pass_through(),
                            );
                        }
                        [] => log::warn!(
                            "tier3_pane::pane_host: GLArea's parent_window has no \
                             INPUT_ONLY child at realize -- expected \
                             priv->event_window (see gtk_gl_area_realize upstream) \
                             was not found. Harmless to pane click/mouse forwarding \
                             (items.id=257 Path B routes that through the frontend's \
                             DOM hit-layer, not this window) -- logged in case a \
                             future GTK version's changed behavior here matters for \
                             some other reason."
                        ),
                        multiple => log::error!(
                            "tier3_pane::pane_host: GLArea's parent_window has \
                             {} INPUT_ONLY children at realize -- expected exactly \
                             one (priv->event_window). Refusing to guess which one \
                             is the real one; none had their input shape reset to \
                             empty. This means GTK's own gtk_gl_area_realize() \
                             behavior has changed from what items.id=227 verified \
                             against (GTK 3.24) -- investigate. Does not affect pane \
                             click/mouse forwarding (items.id=257 Path B routes that \
                             through the frontend's DOM hit-layer, not this window).",
                            multiple.len()
                        ),
                    }
                } else {
                    log::warn!(
                        "tier3_pane::pane_host: GLArea has no GdkWindow at realize -- \
                         cannot reassert pass_through"
                    );
                }
                let width = glarea_for_realize.allocated_width().max(1) as u32;
                let height = glarea_for_realize.allocated_height().max(1) as u32;
                // One `GlProcLoader`, stored in `gl_loader` for the GLArea's
                // whole realized lifetime (see the ROOT CAUSE FOUND comment
                // near this closure's construction) -- both `RenderState`
                // and the standalone `glow::Context` below resolve their
                // function pointers from this same still-alive instance
                // instead of two that would otherwise be dropped (and
                // `dlclose`d) the moment this closure returns.
                *gl_loader.borrow_mut() = Some(crate::tier3_pane::gl_loader::GlProcLoader::open());
                let loader_ref = gl_loader.borrow();
                let loader = loader_ref.as_ref().expect("just set above");
                let state =
                    pollster::block_on(RenderState::new(loader.loader_fn(), (width, height)));
                *render_state.borrow_mut() = Some(state);
                let gl =
                    unsafe { glow::Context::from_loader_function(loader.loader_fn()) };
                *gl_context.borrow_mut() = Some(gl);
                drop(loader_ref);
                log::info!("tier3_pane::pane_host: shared RenderState constructed from GTK's external GL context ({width}x{height})");
            });
        }

        {
            let render_state = render_state.clone();
            let manager = manager.clone();
            let app_handle = app_handle.clone();
            glarea.connect_resize(move |area, width, height| {
                log::debug!(
                    "DIAG items.id=227: connect_resize fired width={width} height={height}"
                );
                let (seq, elapsed_ms) = diag_312_tick();
                log::debug!(
                    "DIAG items.id=312: seq={seq} t={elapsed_ms}ms connect_resize \
                     width={width} height={height} allocated=({},{})",
                    area.allocated_width(),
                    area.allocated_height(),
                );
                let (width, height) = (width.max(1) as u32, height.max(1) as u32);
                if let Some(rs) = render_state.borrow_mut().as_mut() {
                    rs.resize((width, height));
                }
                let layout = app_handle
                    .state::<PaneLayoutState>()
                    .0
                    .lock()
                    .unwrap()
                    .clone();
                let mut mgr = manager.borrow_mut();
                mgr.sync_pane_sizes((width, height), &layout);
                // items.id=234: popup rects rescale proportionally on
                // ordinary window resize too, same mechanism as panes.
                mgr.sync_popup_sizes((width, height));
            });
        }

        {
            let render_state = render_state.clone();
            let gl_context = gl_context.clone();
            let manager = manager.clone();
            let app_handle = app_handle.clone();
            glarea.connect_render(move |area, _gtk_gl_context| {
                use glow::HasContext as _;
                log::debug!(
                    "DIAG items.id=227: connect_render fired, open_panes={}",
                    manager.borrow().panes.len()
                );
                let (seq, elapsed_ms) = diag_312_tick();
                log::debug!(
                    "DIAG items.id=312: seq={seq} t={elapsed_ms}ms connect_render \
                     allocated=({},{})",
                    area.allocated_width(),
                    area.allocated_height(),
                );

                // CRITICAL (confirmed this session): capture GTK's real
                // bound draw framebuffer BEFORE any wgpu device/queue call.
                // wgpu-hal's own internal calls silently rebind
                // GL_DRAW_FRAMEBUFFER to their own scratch target -- GTK's
                // own compositing would read from the wrong framebuffer
                // once this callback returns otherwise. Explicitly rebound
                // below, after every wgpu call this frame.
                let captured_fbo = gl_context
                    .borrow()
                    .as_ref()
                    .map(|gl| unsafe { gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) });

                let layout = app_handle
                    .state::<PaneLayoutState>()
                    .0
                    .lock()
                    .unwrap()
                    .clone();
                let glarea_size = (
                    area.allocated_width().max(1) as u32,
                    area.allocated_height().max(1) as u32,
                );
                // `allocated_width`/`allocated_height` are GTK *logical*
                // pixels; `sync_pane_sizes`/`sync_popup_sizes` (and CEF's
                // `was_resized()`/browser_size contract generally) expect
                // *physical* GL framebuffer pixels, same as `connect_resize`
                // passes them (see the DIAG items.id=312 comment below,
                // which already draws this same distinction for `rs.size()`
                // comparison) -- scale up before using for anything CEF-
                // facing, not just the diagnostic.
                let scale_u32 = area.scale_factor().max(1) as u32;
                let glarea_size_physical = (glarea_size.0 * scale_u32, glarea_size.1 * scale_u32);

                manager.borrow_mut().drain_ready_browsers();

                // FIX (items.id=329, click-sync-on-open): `connect_resize`
                // was the only call site for `sync_pane_sizes`/
                // `sync_popup_sizes`, so a pane opened between two window
                // resizes painted at the right spot (Y-flip fix, items.id=257
                // round 1) but CEF's own notion of that pane's rect stayed
                // whatever it was initialized to until an incidental resize
                // finally synced it -- clicks landed up-and-to-the-right of
                // the visible target until then. `connect_render` already
                // recomputes `layout`/`glarea_size` every tick and fires on
                // `queue_render()` (see `PaneHost::queue_draw`'s doc), which
                // `set_pane_layout` (commands/tier3_pane.rs) already calls
                // right after a pane's first layout fractions land -- so
                // syncing here closes the gap on the very next render tick
                // after open, no new call site needed. Cheap on every other
                // frame: both syncs no-op via `last_applied_size` once a
                // pane/popup's CEF-side size already matches.
                //
                // Round-2 fix-of-a-fix (items.id=329, same session): this
                // block's first version passed the *logical* `glarea_size`
                // here instead of `glarea_size_physical` -- confirmed live,
                // that made every pane render zoomed in (browser_size came
                // out smaller than the real canvas by the scale factor, so
                // CEF laid out a smaller viewport stretched over the actual
                // larger one) and froze scrolling near the page's true
                // bottom (CEF's own scroll-clamp math was working off that
                // wrong, too-small viewport height).
                {
                    let mut mgr = manager.borrow_mut();
                    mgr.sync_pane_sizes(glarea_size_physical, &layout);
                    mgr.sync_popup_sizes(glarea_size_physical);
                }

                // items.id=234: resolve any new popup requests, react to
                // popup lifecycle events (self-close/first-paint-ready),
                // and act on any parent-navigate-away close requests --
                // then forward whatever happened this tick to the
                // frontend as tier3-popup-opened/-closed events.
                let popup_notifications = {
                    let mut mgr = manager.borrow_mut();
                    let mut n = mgr.drain_popup_requests(glarea_size, &layout);
                    n.extend(mgr.drain_popup_events());
                    n.extend(mgr.drain_popup_close_requests());
                    n
                };
                if !popup_notifications.is_empty() {
                    use tauri::Emitter;
                    for notification in popup_notifications {
                        match notification {
                            PopupNotification::Opened { key, rect } => {
                                let payload = PopupOpenedPayload {
                                    provider_id: key,
                                    rect,
                                };
                                if let Err(e) = app_handle.emit("tier3-popup-opened", &payload) {
                                    log::warn!(
                                        "tier3_pane::pane_host: failed to emit \
                                         tier3-popup-opened: {e}"
                                    );
                                }
                            }
                            PopupNotification::Closed { key } => {
                                let payload = PopupClosedPayload { provider_id: key };
                                if let Err(e) = app_handle.emit("tier3-popup-closed", &payload) {
                                    log::warn!(
                                        "tier3_pane::pane_host: failed to emit \
                                         tier3-popup-closed: {e}"
                                    );
                                }
                            }
                        }
                    }
                }

                for pane in manager.borrow().panes.values() {
                    if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
                        host.send_external_begin_frame();
                    }
                }
                for popup in manager.borrow().popups.values() {
                    if let Some(host) = popup.lifecycle.browser().and_then(|b| b.host()) {
                        host.send_external_begin_frame();
                    }
                }

                if let Some(rs) = render_state.borrow_mut().as_mut() {
                    // DIAG items.id=312 (temporary): `glarea_size` above is
                    // GTK *logical* pixels (`allocated_width/height`);
                    // `rs.size()` is whatever the last `connect_resize`
                    // call stored, which GTK documents as *physical* GL
                    // framebuffer pixels -- i.e. already multiplied by the
                    // device scale factor. Compare against
                    // `glarea_size_physical` (computed above, now shared
                    // with the items.id=329 pane/popup size sync) so this
                    // only fires on a genuine staleness mismatch (RenderState
                    // about to wrap a texture at a size GTK's own allocation
                    // has already moved past), not on the expected HiDPI
                    // unit difference.
                    let rs_size = rs.size();
                    if rs_size != glarea_size_physical {
                        log::warn!(
                            "DIAG items.id=312: seq={seq} t={elapsed_ms}ms SIZE MISMATCH \
                             glarea_size_physical={glarea_size_physical:?} (logical={glarea_size:?} \
                             scale={scale_u32}) render_state.size={rs_size:?}"
                        );
                    } else {
                        log::debug!(
                            "DIAG items.id=312: seq={seq} t={elapsed_ms}ms pre-render \
                             size={rs_size:?} (matches glarea_size_physical, scale={scale_u32})"
                        );
                    }
                    let popup_layout: HashMap<PaneKey, PaneRectFraction> = manager
                        .borrow()
                        .popups
                        .iter()
                        .map(|(k, p)| (k.clone(), p.rect))
                        .collect();
                    rs.render(&layout, &popup_layout, captured_fbo.map(|fbo| fbo as u32));
                }

                if let (Some(gl), Some(fbo)) = (gl_context.borrow().as_ref(), captured_fbo) {
                    let framebuffer =
                        std::num::NonZeroU32::new(fbo as u32).map(glow::NativeFramebuffer);
                    unsafe {
                        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, framebuffer);
                        // items.id=257: wgpu's render pass leaves
                        // SCISSOR_TEST enabled and the viewport clamped to
                        // whichever pane was drawn last -- reset both so
                        // nothing else sharing this GL context (GTK's own
                        // subsequent presentation, any other widget drawing
                        // through it) inherits a stale scissor/viewport rect.
                        gl.disable(glow::SCISSOR_TEST);
                        if let Some(rs) = render_state.borrow().as_ref() {
                            let (w, h) = rs.size();
                            gl.viewport(0, 0, w as i32, h as i32);
                        }
                    }
                }
                glib::Propagation::Stop
            });
        }

        // Realize/map/show happens LAST, now that every signal handler
        // above is already connected -- see the ROOT CAUSE FOUND comment
        // near this function's start.
        vbox.add(&overlay);
        overlay.show_all();
        log::info!(
            "tier3_pane::pane_host: post-show_all state -- overlay(realized={}, mapped={}, visible={}) glarea(realized={}, mapped={}, visible={}) webview(realized={}, mapped={}, visible={})",
            overlay.is_realized(), overlay.is_mapped(), overlay.get_visible(),
            glarea.is_realized(), glarea.is_mapped(), glarea.get_visible(),
            webview_widget.is_realized(), webview_widget.is_mapped(), webview_widget.get_visible(),
        );

        Self {
            glarea,
            render_state,
            manager,
            open_pane_count,
        }
    }

    /// Dispatches one `PaneCommand` -- called from `commands::tier3_pane`'s
    /// async IPC handlers via `AppHandle::run_on_main_thread`, which is the
    /// real cross-thread mechanism now (previously a fire-and-forget
    /// `EventLoopProxy::send_event` into a winit user-event queue that
    /// needed a *separate* explicit wake call to actually get dispatched
    /// promptly -- see the old design's `open_tier3_panes` doc for the wake
    /// bug that required). `run_on_main_thread` already guarantees this
    /// runs on the GTK main thread before returning control, so there is no
    /// analogous wake-timing gap here.
    pub fn dispatch(&self, command: PaneCommand) {
        match command {
            PaneCommand::Open { key, url } => {
                let render_state = match self.render_state_for_open() {
                    Some(rs) => rs,
                    None => {
                        log::warn!(
                            "tier3_pane::pane_host: open requested before shared RenderState \
                             was ready (GLArea not realized yet) -- pane={key} not opened"
                        );
                        return;
                    }
                };
                let (device, queue, scale, size) = render_state;
                // `open_pane`'s own already-open guard (below) handles a
                // caller re-`Open`ing an already-open pane -- the frontend
                // does this routinely, confirmed by hand this session, a
                // normal, existing occurrence. No pre-check needed here
                // anymore: unlike the superseded sibling-widget design,
                // nothing is built or added to any widget tree before
                // `open_pane` runs, so there's nothing left to orphan.
                self.manager
                    .borrow_mut()
                    .open_pane(key, url, &device, &queue, scale, size);
                log::debug!("DIAG items.id=227: dispatch(Open) -> queue_draw()");
                self.queue_draw();
            }
            PaneCommand::Close { key } => {
                self.manager.borrow_mut().close_pane(&key);
                log::debug!("DIAG items.id=227: dispatch(Close) -> queue_draw()");
                self.queue_draw();
            }
            PaneCommand::SetActivePane { key } => {
                self.manager.borrow_mut().set_active_pane(key);
                log::debug!("DIAG items.id=359: dispatch(SetActivePane) -> queue_draw()");
                self.queue_draw();
            }
            PaneCommand::MouseClick {
                key,
                x,
                y,
                button,
                mouseup,
                click_count,
                buttons,
                modifiers,
            } => {
                let mut mgr = self.manager.borrow_mut();
                if !mouseup {
                    mgr.focused_pane = Some(key.clone());
                }
                if let Some(host) = mgr
                    .panes
                    .get(&key)
                    .and_then(|p| p.browser_lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    // items.id=313: signal CEF's own focus state on
                    // mousedown, folded in alongside items.id=332's keyboard
                    // forwarding per 313's own deferral note. Only on press,
                    // not release -- matches the `focused_pane` assignment
                    // above, which is also press-only.
                    if !mouseup {
                        host.set_focus(true as _);
                    }
                    let cef_button = match button {
                        PaneMouseButton::Left => MouseButtonType::LEFT,
                        PaneMouseButton::Middle => MouseButtonType::MIDDLE,
                        PaneMouseButton::Right => MouseButtonType::RIGHT,
                    };
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            buttons,
                        ),
                    };
                    host.send_mouse_click_event(
                        Some(&ev),
                        cef_button,
                        mouseup as _,
                        click_count as _,
                    );
                }
            }
            PaneCommand::KeyEvent {
                key,
                event_type,
                windows_key_code,
                character,
                modifiers,
            } => {
                let mut mgr = self.manager.borrow_mut();
                mgr.focused_pane = Some(key.clone());
                if let Some(host) = mgr
                    .panes
                    .get(&key)
                    .and_then(|p| p.browser_lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    let cef_type = match event_type {
                        PaneKeyEventType::RawKeyDown => KeyEventType::RAWKEYDOWN,
                        PaneKeyEventType::Char => KeyEventType::CHAR,
                        PaneKeyEventType::KeyUp => KeyEventType::KEYUP,
                    };
                    let ev = KeyEvent {
                        size: std::mem::size_of::<KeyEvent>(),
                        type_: cef_type,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            0,
                        ),
                        windows_key_code,
                        // items.id=332: no real native/hardware keycode is
                        // available here (input arrives over IPC from a DOM
                        // event, not a native GTK/X11 event) -- mirroring
                        // windows_key_code is a documented best-effort
                        // stand-in, not a claim this is a real scancode.
                        native_key_code: windows_key_code,
                        is_system_key: 0,
                        character,
                        unmodified_character: character,
                        focus_on_editable_field: 0,
                    };
                    host.send_key_event(Some(&ev));
                }
            }
            PaneCommand::MouseMove {
                key,
                x,
                y,
                leaving,
                buttons,
                modifiers,
            } => {
                let mgr = self.manager.borrow();
                if let Some(host) = mgr
                    .panes
                    .get(&key)
                    .and_then(|p| p.browser_lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            buttons,
                        ),
                    };
                    host.send_mouse_move_event(Some(&ev), leaving as _);
                }
            }
            PaneCommand::MouseWheel {
                key,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                let mgr = self.manager.borrow();
                if let Some(host) = mgr
                    .panes
                    .get(&key)
                    .and_then(|p| p.browser_lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            0,
                        ),
                    };
                    // FIX (items.id=329): CEF's wheel-delta convention is
                    // inverted relative to the DOM `WheelEvent.deltaY/deltaX`
                    // this value traces back to (PaneHitLayer.tsx's
                    // `wheelDeltaPixels` -> `forward_pane_mouse_wheel`) --
                    // confirmed live on Claude.ai, scroll direction was
                    // backwards. The superseded Path A code (dead,
                    // `build_pane_hit_widget`) flagged this exact sign as an
                    // unverified assumption; nobody had scroll-tested it
                    // until now.
                    host.send_mouse_wheel_event(
                        Some(&ev),
                        -delta_x.round() as i32,
                        -delta_y.round() as i32,
                    );
                }
            }
            // items.id=234: identical to the MouseClick/MouseMove/MouseWheel
            // arms above, looking up mgr.popups instead of mgr.panes.
            PaneCommand::PopupMouseClick {
                key,
                x,
                y,
                button,
                mouseup,
                click_count,
                buttons,
                modifiers,
            } => {
                let mut mgr = self.manager.borrow_mut();
                if !mouseup {
                    mgr.focused_pane = Some(key.clone());
                }
                if let Some(host) = mgr
                    .popups
                    .get(&key)
                    .and_then(|p| p.lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    // items.id=313: same focus signal as the pane MouseClick
                    // arm above.
                    if !mouseup {
                        host.set_focus(true as _);
                    }
                    let cef_button = match button {
                        PaneMouseButton::Left => MouseButtonType::LEFT,
                        PaneMouseButton::Middle => MouseButtonType::MIDDLE,
                        PaneMouseButton::Right => MouseButtonType::RIGHT,
                    };
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            buttons,
                        ),
                    };
                    host.send_mouse_click_event(
                        Some(&ev),
                        cef_button,
                        mouseup as _,
                        click_count as _,
                    );
                }
            }
            PaneCommand::PopupMouseMove {
                key,
                x,
                y,
                leaving,
                buttons,
                modifiers,
            } => {
                let mgr = self.manager.borrow();
                if let Some(host) = mgr
                    .popups
                    .get(&key)
                    .and_then(|p| p.lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            buttons,
                        ),
                    };
                    host.send_mouse_move_event(Some(&ev), leaving as _);
                }
            }
            PaneCommand::PopupMouseWheel {
                key,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => {
                let mgr = self.manager.borrow();
                if let Some(host) = mgr
                    .popups
                    .get(&key)
                    .and_then(|p| p.lifecycle.browser())
                    .and_then(|b| b.host())
                {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_modifiers_from_dom(
                            modifiers.shift,
                            modifiers.ctrl,
                            modifiers.alt,
                            modifiers.meta,
                            0,
                        ),
                    };
                    // FIX (items.id=329): see the `PaneCommand::MouseWheel`
                    // arm above -- same inverted-sign bug, same fix.
                    host.send_mouse_wheel_event(
                        Some(&ev),
                        -delta_x.round() as i32,
                        -delta_y.round() as i32,
                    );
                }
            }
        }
    }

    /// Pulls what `open_pane` needs from the shared `RenderState`
    /// (device/queue clones -- both cheap, `wgpu::Device`/`Queue` are
    /// themselves `Arc`-backed handles) plus the GLArea's own current
    /// scale/size for this new pane's initial `PaneRenderHandler` size.
    /// `None` if the GLArea hasn't realized yet (its GL context, and
    /// therefore the shared `RenderState`, doesn't exist until then) --
    /// shouldn't happen in practice (the main window realizes long before
    /// any pane can be opened), but not assumed.
    fn render_state_for_open(&self) -> Option<(wgpu::Device, wgpu::Queue, f32, LogicalSize)> {
        let scale = self.glarea.scale_factor().max(1) as f32;
        // items.id=328: the `LogicalSize` returned here becomes this pane's
        // *initial* `PaneRenderHandler.size`, which feeds CEF's
        // `GetViewRect` directly -- confirmed live that CEF wants the real
        // physical pixel size there (see `sync_pane_sizes`'s own comment for
        // the full finding), not DIP. `allocated_width`/`allocated_height`
        // are GTK *logical* pixels (`connect_render`'s own comment), so
        // scale up to physical here, same as `glarea_size_physical`
        // elsewhere in this file. Self-corrects within this pane's first
        // `sync_pane_sizes` tick regardless (its `last_applied_size` starts
        // `None`), but there's no reason to seed CEF's very first
        // `GetViewRect` with a wrong value on purpose.
        let width = self.glarea.allocated_width().max(1) as f32 * scale;
        let height = self.glarea.allocated_height().max(1) as f32 * scale;
        self.render_state.borrow().as_ref().map(|rs| {
            (
                rs.device(),
                rs.queue(),
                scale,
                LogicalSize { width, height },
            )
        })
    }

    /// Every currently-open pane's key, in open order.
    pub fn pane_keys(&self) -> Vec<PaneKey> {
        self.manager.borrow().panes.keys().cloned().collect()
    }

    /// Shared with main.rs's GLib timeout -- see `PaneManager::open_pane_count`'s
    /// doc for why this is a live count, not a one-shot latch.
    pub fn open_pane_count(&self) -> Arc<AtomicUsize> {
        self.open_pane_count.clone()
    }

    /// Requests the next GTK frame draw this pane host's GLArea -- called
    /// from main.rs's GLib timeout, gated on `open_pane_count() > 0`, and
    /// from `set_pane_layout` (commands/tier3_pane.rs) for an immediate
    /// resync the moment the frontend reports new layout fractions, same
    /// intent as the old design's immediate `sync_tx` push. No longer also
    /// rebuilds a GDK input shape (items.id=257 Path A, removed) -- pane
    /// click/mouse routing is IPC-driven now (Path B, see `dispatch`'s
    /// `PaneCommand::MouseClick`/`MouseMove`/`MouseWheel` arms), with
    /// nothing left here for a layout change to resync.
    ///
    /// items.id=257 Failure 2 root cause (2026-08-28): `glarea` is
    /// configured with `set_auto_render(false)` (see its construction
    /// above), which per GtkGLArea's own documented contract means the
    /// `render` signal fires ONLY on an explicit `gtk_gl_area_queue_render()`
    /// call or an actual window resize -- a plain `queue_draw()` just
    /// re-composites whatever was last rendered. This called `queue_draw()`
    /// (`GtkWidget`'s generic, unconditional invalidate) instead, so once
    /// window-resize events stopped, `render` stopped firing permanently,
    /// confirmed live: 0 `connect_render` firings across 2000+ `queue_draw()`
    /// calls and 780+ timer ticks with a pane open, resuming instantly only
    /// for the duration of an interactive resize. `queue_render()` is
    /// `GtkGLArea`'s own API for exactly this -- it marks the previous
    /// render invalid AND queues the draw, guaranteeing `render` fires.
    pub fn queue_draw(&self) {
        log::debug!("DIAG items.id=227: PaneHost::queue_draw() called");
        self.glarea.queue_render();
    }
}

// ---------------------------------------------------------------------------
// Main-thread-only global access
// ---------------------------------------------------------------------------
//
// `PaneHost` (GTK objects throughout) is not `Send`, so it cannot be reached
// via `tauri::State` from `commands::tier3_pane`'s async (tokio-side)
// handlers the way the old `EventLoopProxy<PaneCommand>` -- Send + Sync by
// winit's own guarantee -- was. The replacement is Tauri's own
// `AppHandle::run_on_main_thread(closure)`: the closure itself must be
// `Send`, but it EXECUTES on the main thread, so it can safely reach a
// thread-local holding the real, non-Send `PaneHost` from inside its own
// body. This is simpler than the old design, not just a substitute for it:
// `run_on_main_thread` is dispatch AND wake in one call (it's Tauri/tao's
// own main-thread queue, serviced as part of GTK's ordinary main-loop
// operation) -- the old design's `EventLoopProxy::send_event` needed a
// *separate*, explicit `run_on_main_thread(|| {})` wake call right after it
// (see the old `open_tier3_panes` doc), because sending into winit's queue
// alone did not guarantee `tao` would notice and dispatch it promptly. That
// whole class of bug has no equivalent here.

thread_local! {
    static HOST: RefCell<Option<PaneHost>> = const { RefCell::new(None) };
}

/// Installs the process-wide pane host. Must run once, on the main thread,
/// after the main Tauri window exists (needs `WebviewWindow::gtk_window()`/
/// `default_vbox()`). Returns the live open-pane-count handle so main.rs's
/// GLib timeout can read it without reaching back into the thread-local.
pub fn install(window: &tauri::WebviewWindow, app_handle: tauri::AppHandle) -> Arc<AtomicUsize> {
    let host = PaneHost::install(window, app_handle);
    let count = host.open_pane_count();
    HOST.with(|h| *h.borrow_mut() = Some(host));
    count
}

/// Runs `command` against the process-wide pane host. Must be called from
/// the main thread -- the intended call site is inside an
/// `AppHandle::run_on_main_thread` closure (see
/// `commands::tier3_pane::open_tier3_panes`/`close_tier3_pane`), which
/// guarantees that. A call before `install()` (shouldn't happen -- the main
/// window exists long before any pane can open) is logged and dropped, not a
/// panic.
pub fn dispatch(command: PaneCommand) {
    HOST.with(|h| match h.borrow().as_ref() {
        Some(host) => host.dispatch(command),
        None => log::warn!("tier3_pane::pane_host: dispatch called before install()"),
    });
}

/// Requests a redraw on the process-wide pane host's GLArea. Same
/// main-thread-only contract as `dispatch`. Used by
/// `commands::tier3_pane::set_pane_layout` for an immediate resync the
/// moment the frontend reports new layout fractions.
pub fn queue_draw() {
    HOST.with(|h| {
        if let Some(host) = h.borrow().as_ref() {
            host.queue_draw();
        }
    });
}

/// Closes every currently-open pane. Same main-thread-only contract as
/// `dispatch` -- the intended (only) call site is main.rs's `RunEvent::Exit`
/// handler (items.id=315), which already runs synchronously on the main
/// thread, so unlike `dispatch`'s usual callers this does not need
/// `run_on_main_thread` wrapping. Must run before `cef::shutdown()`: closing
/// each pane's browser here is what lets CEF release its own GPU/Vulkan
/// resources cleanly, rather than tao's unconditional `process::exit()`
/// tearing them down while a browser instance is still alive.
pub fn close_all_panes() {
    HOST.with(|h| {
        if let Some(host) = h.borrow().as_ref() {
            for key in host.pane_keys() {
                host.dispatch(PaneCommand::Close { key });
            }
        }
    });
}

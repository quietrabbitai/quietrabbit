//! Manually-synced CEF panes -- multi-pane, on-demand (items.id=202 piece 5,
//! items.id=223).
//!
//! Architecture decided 2026-08-01 (see mod.rs docs): no supported path
//! exists to composite CEF's OSR output into Tauri's own webview surface,
//! and native OS-level child-window reparenting is unavailable on Wayland.
//! This module owns a single shared `winit` `EventLoop`, manually keeping
//! each pane's window in sync with the main Tauri window's position/size
//! via events forwarded from the Tauri side (see `PaneWindow::sync_to`).
//!
//! # Phase B architecture (this revision)
//! Phase A built exactly one pane, created eagerly at startup, never closed.
//! Phase B generalizes to 1-3 panes (decisions.id=681's cap), created and
//! destroyed on demand:
//!
//! - **One shared `EventLoop<PaneCommand>`, many `Window`s.** winit 0.30.13
//!   guards `EventLoop::new()`/`.build()` with a *process-global*
//!   `AtomicBool` (`EVENT_LOOP_CREATED`, `winit::event_loop` source) --
//!   confirmed directly against winit's own source, not assumed: a second
//!   construction anywhere in the process fails with
//!   `EventLoopError::RecreationAttempt`, even on another thread. One
//!   `EventLoop` per pane is therefore not an option. Instead this follows
//!   winit's own canonical multi-window pattern (`examples/window.rs`):
//!   one `ApplicationHandler` (`PaneManager`, below) owns every pane's
//!   `Window`, dispatched by `WindowId` in `window_event`.
//! - **On-demand creation via a custom user event.** `PaneCommand::Open`/
//!   `Close`, delivered through an `EventLoopProxy<PaneCommand>` --
//!   `ActiveEventLoop::create_window()` is callable from `user_event()`
//!   just as validly as from `resumed()` (confirmed against winit's source
//!   and its own multi-window example, which calls `create_window` from a
//!   `window_event` handler). `EventLoopProxy` is `Send + Sync` (unlike
//!   `PaneManager`/`PaneWindow` themselves, kept off `tauri::State` for the
//!   same non-`Send` reason documented in main.rs -- X11 IME pointers), so
//!   it is what lets an async Tauri IPC command handler (running on tokio,
//!   nowhere near this module's winit-thread affinity) request a pane
//!   open/close at all. This directly implements items.id=223: nothing is
//!   created until an `Open` command arrives, so a user who never touches
//!   Tier 2/3 pays none of Phase A's always-on cost.
//! - **Pane key = provider ID**, not an index or UUID -- see
//!   `tier3_pane::PaneKey`'s own doc for the reasoning.
//!
//! Phase A's per-pane mechanics (manual sync, debounced resize, deferred
//! async browser creation, the freeze-bug heartbeat this module doesn't
//! itself own) are all still exactly as proven -- this revision keys and
//! multiplies that machinery, it does not redesign it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use cef::{ImplBrowser, ImplBrowserHost, ImplFrame};

use crate::tier3_pane::render::{ClientBuilder, PaneRenderHandler, RenderState};
use crate::tier3_pane::PaneKey;

/// Lifecycle of one pane's CEF browser, replacing the old
/// `browser: Option<cef::Browser>` + `browser_created: bool` pair
/// (items.id=204). items.id=204's own design already anticipated Phase B:
/// wrap multiple `BrowserLifecycle` instances in a keyed map, but keep that
/// map inside the same non-`Send` ownership boundary this module's
/// `PaneApp` (now `PaneManager`) occupied even in Phase A -- not a global
/// `static`, not `tauri::State`. `PaneState`, below, is that map's value
/// type.
#[allow(dead_code)] // Closing/Closed: no code path drives these yet --
                    // close_pane (this revision) goes straight from Ready
                    // to removing the PaneState entirely rather than
                    // passing through this enum's own Closing/Closed
                    // states. Kept per the finalized design; wire them if
                    // a graceful (non-forced) close path is ever needed.
enum BrowserLifecycleState {
    Uninitialized,
    Creating,
    Ready(cef::Browser),
    Closing,
    Closed,
    Failed(String),
}

/// Deferred until the browser is `Ready`; applied immediately if it
/// already is. Failure policy (items.id=204, resolved): every failed
/// apply gets an unconditional log::warn! -- no retry, no completion
/// signal yet (that's explicitly Phase B scope).
#[derive(Debug)]
#[allow(dead_code)] // SetCookie: explicit stub per the finalized design --
                    // no cookie-manager API is wired in this codebase yet
                    // (Phase B, see mod.rs), so nothing constructs this
                    // variant. apply_action already returns Err for it.
enum PendingAction {
    Navigate(String),
    SetCookie { name: String, value: String },
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
    /// called while `Uninitialized` -- mirrors the old `browser_created`
    /// one-shot flag exactly.
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

    /// Called from `PaneWindow::pump()` when a pane's `browser_ready_rx`
    /// yields a `Browser` -- transitions to `Ready` and drains anything
    /// queued while `Uninitialized`/`Creating`.
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
            Err("SetCookie not yet supported -- Phase B cookie-jar wiring (see mod.rs)".to_string())
        }
    }
}

/// Commands sent into `PaneManager` from outside the winit-thread-affine
/// boundary (e.g. a Tauri IPC command handler running on tokio) via
/// `EventLoopProxy::send_event`. This is items.id=223's actual lifecycle
/// hook -- nothing in this module creates a pane's window, `RequestContext`,
/// or CEF browser except in response to `Open`; nothing happens eagerly at
/// startup (see `PaneWindow::new`).
#[derive(Debug, Clone)]
pub enum PaneCommand {
    Open { key: PaneKey, url: String },
    Close { key: PaneKey },
}

/// Per-pane state: today's Phase A `PaneApp` fields, unchanged in shape,
/// (no per-provider `RequestContext` -- items.id=224 resolution,
/// decisions.id=711; every pane uses CEF's one global context).
struct PaneState {
    window: Option<Arc<Window>>,
    render_state: Option<RenderState>,
    browser_lifecycle: BrowserLifecycle,
    // Arc<Mutex<>>, not Rc<RefCell<>> -- shared with PaneRenderHandler, whose
    // view_rect runs on CEF's UI thread while apply_resize (below) writes
    // this on the main thread. See PaneRenderHandler::size docs (render.rs)
    // for why this must not be an Rc<RefCell<>> under
    // multi_threaded_message_loop=true (items.id=203 audit, 2026-08-03).
    pending_render_handler: Option<(PaneRenderHandler, Arc<Mutex<winit::dpi::LogicalSize<f32>>>)>,
    browser_size: Option<Arc<Mutex<winit::dpi::LogicalSize<f32>>>>,
    window_info_and_settings: Option<(cef::WindowInfo, cef::BrowserSettings)>,
    /// Receives the constructed `Browser` from `LifeSpanHandler::on_after_created`
    /// (see render.rs docs) -- CEF's UI thread delivers it here since
    /// creation is async under multi_threaded_message_loop. Drained in
    /// `PaneWindow::pump()`, same as `window_event` handles other
    /// cross-boundary events.
    browser_ready_rx: Option<std::sync::mpsc::Receiver<cef::Browser>>,
    /// The physical size `sync_to()` last explicitly requested via
    /// `request_inner_size`. Compared against actual `WindowEvent::Resized`
    /// sizes to detect external interference (user maximize/tile/manual
    /// resize) -- see `PaneWindow::sync_to`'s doc for why this exists.
    last_requested_size: Rc<RefCell<Option<winit::dpi::PhysicalSize<u32>>>>,
    /// Debounced resize target: the most recent size reported by
    /// `WindowEvent::Resized`, applied to render_state/CEF only once no
    /// further Resized events arrive for `RESIZE_DEBOUNCE` -- see
    /// `window_event`'s `Resized` arm doc for why this exists (found the
    /// hard way, 2026-08-01: a compositor-driven maximize/tile gesture
    /// fires many rapid Resized events, and processing each one
    /// synchronously -- Vulkan surface reconfiguration plus a cross-thread
    /// call into CEF's now-separate UI thread -- stalled the pane's event
    /// loop entirely during the gesture).
    pending_resize: Option<winit::dpi::PhysicalSize<u32>>,
    last_resize_event_at: Option<std::time::Instant>,
    /// URL loaded on first browser creation -- the provider's own
    /// `launch_url` (`provider_store::Provider`), resolved server-side by
    /// the IPC command that issues this pane's `PaneCommand::Open`. That
    /// same command handler restores this provider's stored cookies into
    /// CEF's global jar (persistence::tier3_cookie_store) before sending
    /// `Open`, so they're already in place by the time this URL loads.
    initial_url: String,
}

impl PaneState {
    /// Actually applies a settled resize: reconfigures the wgpu surface
    /// and notifies CEF's browser host. Called from `PaneWindow::pump()`
    /// once `RESIZE_DEBOUNCE` has elapsed since the last raw `Resized`
    /// event -- see that method and `pending_resize`'s docs for why this
    /// is deferred rather than applied directly in `window_event`.
    fn apply_resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if let Some(render_state) = self.render_state.as_mut() {
            render_state.resize(size);
        }
        if let (Some(browser_size), Some(window)) =
            (self.browser_size.as_ref(), self.window.as_ref())
        {
            *browser_size.lock().unwrap() = size.to_logical(window.scale_factor());
            if let Some(host) = self.browser_lifecycle.browser().and_then(|b| b.host()) {
                host.was_resized();
            }
        }
    }
}

/// How long to wait after the last `Resized` event before actually
/// reconfiguring the wgpu surface and notifying CEF. 100ms is comfortably
/// longer than a single frame at any reasonable refresh rate, so it won't
/// visibly delay a genuine one-shot resize, but long enough to coalesce a
/// rapid-fire sequence from a compositor animation into one final apply.
const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

/// One shared `ApplicationHandler`, owning every currently-open pane.
struct PaneManager {
    /// `IndexMap`, not `HashMap`: insertion order == pane open order,
    /// which `PaneWindow::sync_to`'s placeholder stacking (below) relies on
    /// to keep multiple panes from landing on top of each other, and which
    /// will matter again for Phase C's real split-screen layout ordering.
    panes: IndexMap<PaneKey, PaneState>,
    /// Reverse lookup -- `window_event` only carries a `WindowId`. Plain
    /// `HashMap`: pure key->value lookup, never iterated.
    window_ids: HashMap<WindowId, PaneKey>,
    /// Live count (not Phase A's one-shot `running` latch) -- incremented
    /// on `Open`, decremented on `Close`/`CloseRequested`. Shared with
    /// main.rs's heartbeat thread so it can gate its real cost (and, by
    /// extension, everything downstream of that heartbeat existing at all)
    /// on whether any pane is actually open right now -- items.id=223.
    open_pane_count: Arc<AtomicUsize>,
}

impl PaneManager {
    fn open_pane(&mut self, event_loop: &ActiveEventLoop, key: PaneKey, url: String) {
        if self.panes.contains_key(&key) {
            log::warn!(
                "tier3_pane::sync_window: open requested for already-open pane {key:?} -- ignoring"
            );
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title(format!("Quiet Rabbit — Tier 2/3 pane ({key})")),
                )
                .expect("tier3_pane::sync_window: failed to create pane window"),
        );

        let render_state = pollster::block_on(RenderState::new(window.clone(), key.clone()));

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
            shared_texture_enabled: accelerated_osr as _,
            external_begin_frame_enabled: accelerated_osr as _,
            ..Default::default()
        };
        let browser_settings = cef::BrowserSettings {
            windowless_frame_rate: 60,
            ..Default::default()
        };

        let device_scale_factor = window.scale_factor();
        let (render_handler, browser_size) = PaneRenderHandler::new(
            render_state.device(),
            render_state.queue(),
            device_scale_factor as f32,
            window.inner_size().to_logical(device_scale_factor),
            key.clone(),
        );

        // No per-pane RequestContext (items.id=224 resolution,
        // decisions.id=711): CEF's Chrome-runtime ChromeBrowserContext
        // structurally rejects any second RequestContext -- confirmed via
        // gdb (src-tauri/examples/repro_224.rs), the first (global,
        // root_cache_path-rooted) context is the only one that ever reaches
        // Chromium's real ProfileManager::CreateProfileAsync; any second
        // context's InitializeAsync self-invokes a synthetic failure
        // without ever calling it, independent of message-pump/threading
        // config. Every pane now uses CEF's one working global context
        // (request_context=None below -- "If |request_context| is NULL the
        // global request context will be used," confirmed against the
        // vendored cef crate's own doc comment). Per-provider cookie
        // *persistence* across app restarts is handled at the QR
        // application layer instead -- see
        // commands/tier3_pane.rs::open_tier3_panes/close_tier3_pane, which
        // load/save via persistence::tier3_cookie_store into/from this one
        // shared jar around this pane's lifetime. Domain-scoping inside
        // that single jar already keeps different providers' cookies apart
        // (the same guarantee two tabs in one ordinary browser profile
        // already rely on) -- no per-pane isolation mechanism is needed.

        self.panes.insert(
            key.clone(),
            PaneState {
                window: Some(window.clone()),
                render_state: Some(render_state),
                browser_lifecycle: BrowserLifecycle::new(),
                pending_render_handler: Some((render_handler, browser_size.clone())),
                browser_size: Some(browser_size),
                window_info_and_settings: Some((window_info, browser_settings)),
                browser_ready_rx: None,
                last_requested_size: Rc::new(RefCell::new(None)),
                pending_resize: None,
                last_resize_event_at: None,
                initial_url: url,
            },
        );
        self.window_ids.insert(window.id(), key);
        self.open_pane_count.fetch_add(1, Ordering::Relaxed);

        window.request_redraw();
    }

    fn close_pane(&mut self, key: &PaneKey) {
        let Some(pane) = self.panes.shift_remove(key) else {
            return;
        };
        if let Some(window_id) = pane.window.as_ref().map(|w| w.id()) {
            self.window_ids.remove(&window_id);
        }
        // Forced close (no unload-confirmation dance): Phase A never closed
        // anything, so there's no prior "graceful close" precedent in this
        // codebase to match -- force_close=true is the safe default for a
        // user-initiated pane close (items.id=223's whole trigger), not a
        // navigation-away the page itself might want to intercept.
        if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
            host.close_browser(true as _);
        }
        crate::tier3_pane::render::remove_pane_texture(key);
        self.open_pane_count.fetch_sub(1, Ordering::Relaxed);
        // pane itself (window, render_state) drops here, tearing down the
        // wgpu device/surface. CEF's global cookie jar is untouched by a
        // pane close -- it's shared across all panes, not pane-scoped (see
        // commands/tier3_pane.rs::close_tier3_pane for where this pane's
        // provider's cookies actually get persisted, before this call).
    }
}

impl ApplicationHandler<PaneCommand> for PaneManager {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // No-op: Phase B creates no panes eagerly (see PaneWindow::new
        // docs and items.id=223) -- all window/RequestContext/browser
        // creation happens in user_event(Open) instead, which winit
        // supports calling create_window from just as validly as from
        // resumed() (confirmed against winit's own source and its
        // multi-window example). Phase A's resumed() needed a
        // has-this-run-before guard because winit calls this whenever it
        // likes across the loop's lifetime (confirmed then: closing the
        // one pane window produced a second resumed() call) and Phase A's
        // version unconditionally rebuilt state on every call; this
        // version has no state to rebuild, so no guard is needed.
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: PaneCommand) {
        match event {
            PaneCommand::Open { key, url } => self.open_pane(event_loop, key, url),
            PaneCommand::Close { key } => self.close_pane(&key),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(key) = self.window_ids.get(&window_id).cloned() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                // Phase A called event_loop.exit() here -- correct when
                // there was only ever one window, wrong now: closing one
                // pane must not take every other open pane (and the
                // heartbeat, and CEF's own process-wide threads) down with
                // it. Tear down just this pane; the shared event loop
                // itself stays alive for the app's lifetime -- nothing in
                // this module drives a full event-loop exit, matching
                // items.id=223's "close just tears that pane's cost down,
                // not the app."
                self.close_pane(&key);
            }
            WindowEvent::RedrawRequested => {
                let Some(pane) = self.panes.get_mut(&key) else {
                    return;
                };
                if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
                    host.send_external_begin_frame();
                }
                if let (Some(render_state), Some(window)) =
                    (pane.render_state.as_mut(), pane.window.as_ref())
                {
                    render_state.render(window);
                    // FOUND THE HARD WAY (2026-08-01, Phase A): do NOT call
                    // window.request_redraw() unconditionally here -- see
                    // PaneWindow::pump()'s own request_redraw() call for
                    // why this pane's redraw cadence is driven externally,
                    // once per pump tick, not self-perpetuated from inside
                    // this handler.
                }

                // start_creation() is the one-shot latch that ensures this
                // fires exactly once per pane. No context-readiness gate
                // needed (Phase B had one here for the now-removed per-pane
                // RequestContext) -- CEF's global context is already fully
                // initialized well before any pane can open at all (it's
                // the context items.id=224's trace confirmed does reach
                // the real ProfileManager, at CEF startup).
                if pane.browser_lifecycle.start_creation() {
                    if let (Some((render_handler, _)), Some((window_info, browser_settings))) = (
                        pane.pending_render_handler.take(),
                        pane.window_info_and_settings.as_ref(),
                    ) {
                        // ASYNC creation (2026-08-01): browser_host_create_browser_sync
                        // requires being called from CEF's own UI thread, which is no
                        // longer this thread under multi_threaded_message_loop (see
                        // render.rs's LifeSpanHandler docs -- found the hard way, real
                        // run returned browser_host_create_browser_sync -> false with
                        // no error). browser_host_create_browser is callable from any
                        // thread and delivers the Browser asynchronously via
                        // on_after_created, received in PaneWindow::pump().
                        let (tx, rx) = std::sync::mpsc::channel();
                        pane.browser_ready_rx = Some(rx);
                        // request_context: None -- CEF's global context is
                        // used (items.id=224 resolution, decisions.id=711).
                        let created = cef::browser_host_create_browser(
                            Some(window_info),
                            Some(&mut ClientBuilder::build(render_handler, tx)),
                            None,
                            Some(browser_settings),
                            None,
                            None,
                        );
                        log::info!(
                            "tier3_pane::sync_window: browser_host_create_browser (async, pane={key}) dispatched -> {created}"
                        );
                        if created != 0 {
                            pane.browser_lifecycle
                                .enqueue(PendingAction::Navigate(pane.initial_url.clone()));
                        } else {
                            pane.browser_lifecycle.fail(
                                "browser_host_create_browser returned false (async creation dispatch failed)",
                            );
                        }
                    }
                }
            }
            WindowEvent::Resized(size) => {
                // Debounced (see PaneState::pending_resize / PaneWindow::pump
                // docs) -- record only, don't act yet. Acting synchronously
                // here on every intermediate event during a compositor
                // resize/maximize gesture is what stalled the pane's event
                // loop (found the hard way, 2026-08-01).
                let Some(pane) = self.panes.get_mut(&key) else {
                    return;
                };
                pane.pending_resize = Some(size);
                pane.last_resize_event_at = Some(std::time::Instant::now());
            }
            _ => {}
        }
    }
}

/// Owns the shared winit event loop and the `PaneManager` that dispatches
/// across every currently-open pane. Constructed once at startup with zero
/// panes (Phase B/items.id=223: on-demand only, nothing eager) -- panes
/// come and go via `request_open`/`request_close` for the app's entire
/// lifetime.
pub struct PaneWindow {
    event_loop: Option<EventLoop<PaneCommand>>,
    manager: PaneManager,
    proxy: EventLoopProxy<PaneCommand>,
    /// Same `Arc` as `PaneManager::open_pane_count` -- exposed here so
    /// main.rs's heartbeat thread (which never touches `PaneManager`
    /// directly, for the same non-`Send` reason `PaneWindow` itself stays
    /// off `tauri::State`) can read it without needing a callback into the
    /// winit-thread-affine side.
    open_pane_count: Arc<AtomicUsize>,
}

impl PaneWindow {
    /// No longer takes `root_cache_path` (items.id=224 resolution,
    /// decisions.id=711) -- `PaneManager` no longer builds a per-pane
    /// `cache_path` under it (there is no per-pane `RequestContext` left
    /// to build one for). `root_cache_path` is still needed by
    /// `bootstrap::initialize_cef` in main.rs -- that's CEF's global
    /// context path, unaffected by this change.
    pub fn new() -> Self {
        let event_loop = EventLoop::<PaneCommand>::with_user_event()
            .build()
            .expect("tier3_pane::sync_window: failed to create winit event loop");
        let proxy = event_loop.create_proxy();
        let open_pane_count = Arc::new(AtomicUsize::new(0));
        Self {
            event_loop: Some(event_loop),
            manager: PaneManager {
                panes: IndexMap::new(),
                window_ids: HashMap::new(),
                open_pane_count: open_pane_count.clone(),
            },
            proxy,
            open_pane_count,
        }
    }

    /// `Send + Sync` (winit's own guarantee) -- clone this into Tauri
    /// managed state so async IPC command handlers can request pane
    /// open/close without needing access to `PaneManager` itself, which
    /// cannot cross that boundary (see module docs).
    pub fn proxy(&self) -> EventLoopProxy<PaneCommand> {
        self.proxy.clone()
    }

    /// Shared with main.rs's heartbeat thread -- see
    /// `PaneManager::open_pane_count`'s doc for why this replaced Phase A's
    /// one-shot `running` flag.
    pub fn open_pane_count(&self) -> Arc<AtomicUsize> {
        self.open_pane_count.clone()
    }

    /// Every currently-open pane's key, in open order. Used by main.rs's
    /// window move/resize forwarding to call `sync_to` for each open pane
    /// without needing direct access to `PaneManager`'s private map.
    pub fn pane_keys(&self) -> Vec<PaneKey> {
        self.manager.panes.keys().cloned().collect()
    }

    /// Runs one non-blocking iteration of the shared winit event loop,
    /// servicing every currently-open pane. Called from Tauri's own
    /// `RunEvent::MainEventsCleared` (see mod.rs docs on the interleaving
    /// architecture).
    ///
    /// One `pump_app_events` call per tick regardless of pane count --
    /// winit dispatches each queued event to whichever window/pane it
    /// actually targets via `WindowId`, so N panes do not need N pump
    /// calls, only the per-pane bookkeeping below does.
    pub fn pump(&mut self) {
        use winit::platform::pump_events::EventLoopExtPumpEvents;

        for pane in self.manager.panes.values_mut() {
            // Deliver any browser CEF's UI thread finished constructing
            // since the last tick (see render.rs's LifeSpanHandler docs for
            // why this is async now).
            if let Some(rx) = pane.browser_ready_rx.as_ref() {
                if let Ok(browser) = rx.try_recv() {
                    log::info!("tier3_pane::sync_window: browser delivered via on_after_created");
                    pane.browser_lifecycle.on_created(browser);
                }
            }

            // Apply a debounced resize once no further Resized events have
            // arrived for RESIZE_DEBOUNCE (see PaneState::pending_resize
            // docs). Checked here rather than in the Resized handler
            // itself, since pump() is what's actually invoked on a steady
            // ~60fps cadence regardless of how many window events arrive
            // in between.
            if let (Some(size), Some(last_event_at)) =
                (pane.pending_resize, pane.last_resize_event_at)
            {
                if last_event_at.elapsed() >= RESIZE_DEBOUNCE {
                    pane.apply_resize(size);
                    pane.pending_resize = None;
                    pane.last_resize_event_at = None;
                }
            }

            if let Some(window) = pane.window.as_ref() {
                window.request_redraw();
            }
        }

        if let Some(event_loop) = self.event_loop.as_mut() {
            // No PumpStatus::Exit handling: individual pane close no
            // longer exits the shared event loop (see window_event's
            // CloseRequested handling above) -- nothing in this module
            // currently drives a full event-loop exit, and the shared
            // loop is meant to outlive any single pane regardless.
            let _ = event_loop.pump_app_events(Some(std::time::Duration::ZERO), &mut self.manager);
        }

        std::thread::sleep(std::time::Duration::from_millis(1000 / 60));
    }

    /// Repositions/resizes one pane's window to sit flush against the
    /// given rect (in physical pixels), expressed relative to the same
    /// screen coordinate space the main Tauri window reports. Called from
    /// Tauri's window move/resize event handlers, once per currently-open
    /// pane (see `pane_keys`) -- see mod.rs docs on why this is manual
    /// rather than OS-level child-window reparenting.
    ///
    /// PLACEHOLDER layout, same discipline as Phase A's single-pane
    /// version: each pane still gets the same fixed placeholder width,
    /// stacked left-to-right by open order (`panes.get_index_of`) so
    /// multiple open panes are at least visually distinguishable rather
    /// than landing on top of each other. This is NOT Phase C's real
    /// split-screen layout math (IA Section 3c's resting/active ratio) --
    /// that is still explicitly out of scope this session (mod.rs docs).
    ///
    /// FOUND THE HARD WAY (2026-08-01): calling this unconditionally on
    /// every tick fights the window manager whenever the user manually
    /// maximizes/tiles/resizes the pane window directly (it has normal
    /// decorations -- min/max/close). `set_outer_position`/
    /// `request_inner_size` both "automatically un-maximize the window if
    /// it's maximized" per winit's own docs -- so a forced sync_to() call
    /// arriving on the very next ~16ms tick after the compositor maximizes
    /// the window immediately un-maximizes it again, and the two fight
    /// continuously. `is_maximized()` is not implemented on Wayland/X11
    /// (per winit's own docs), so maximize state can't be queried
    /// directly. Instead: skip the forced sync whenever the pane's actual
    /// size doesn't match what `sync_to()` itself last requested -- a
    /// mismatch means something else (the user, the compositor) changed it
    /// since, and forcing our own geometry back is exactly the behavior
    /// that caused the freeze. Once the size matches again (e.g. the user
    /// un-maximizes/restores it), syncing resumes automatically on the
    /// next call.
    pub fn sync_to(&self, key: &PaneKey, main_window_rect: PhysicalRect) {
        let Some(index) = self.manager.panes.get_index_of(key) else {
            return;
        };
        let Some(pane) = self.manager.panes.get(key) else {
            return;
        };
        let Some(window) = pane.window.as_ref() else {
            return;
        };

        let actual_size = window.inner_size();
        let expected = *pane.last_requested_size.borrow();
        if let Some(expected) = expected {
            if actual_size != expected {
                // Something external changed the pane's size since we last
                // set it -- back off rather than fight it.
                return;
            }
        }

        const PLACEHOLDER_PANE_WIDTH: i32 = 480;
        let pane_x =
            main_window_rect.x + main_window_rect.width + (index as i32 * PLACEHOLDER_PANE_WIDTH);
        window.set_outer_position(winit::dpi::PhysicalPosition::new(
            pane_x,
            main_window_rect.y,
        ));
        let new_size = winit::dpi::PhysicalSize::new(
            PLACEHOLDER_PANE_WIDTH as u32,
            main_window_rect.height as u32,
        );
        let _ = window.request_inner_size(new_size);
        *pane.last_requested_size.borrow_mut() = Some(new_size);
    }
}

/// Physical-pixel rect, screen-space. Mirrors what Tauri's `Window::outer_position`
/// / `Window::outer_size` report -- defined here rather than importing a
/// Tauri type directly, so this module stays testable without a Tauri
/// window present (matches the existing codebase's pattern in
/// ollama_sidecar.rs of keeping platform-adjacent modules free of
/// Tauri-specific types where practical).
#[derive(Clone, Copy, Debug)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

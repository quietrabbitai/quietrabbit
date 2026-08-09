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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gtk::prelude::*;
use indexmap::IndexMap;
use tauri::Manager;

use cef::{ImplBrowser, ImplBrowserHost, ImplFrame};

use crate::commands::tier3_pane::{PaneLayoutState, PaneRectFraction};
use crate::tier3_pane::render::{ClientBuilder, LogicalSize, PaneRenderHandler, RenderState};
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
    }
}

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
            shared_texture_enabled: accelerated_osr as _,
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
            Some(&mut ClientBuilder::build(render_handler, tx)),
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
        let Some(pane) = self.panes.shift_remove(key) else {
            return;
        };
        // Forced close (no unload-confirmation dance) -- a user-initiated
        // pane close (items.id=223's whole trigger), not a navigation-away
        // the page itself might want to intercept.
        if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
            host.close_browser(true as _);
        }
        crate::tier3_pane::render::remove_pane_texture(key);
        self.open_pane_count.fetch_sub(1, Ordering::Relaxed);
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
        scale_factor: f32,
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
            *pane.browser_size.lock().unwrap() = LogicalSize {
                width: width as f32 / scale_factor,
                height: height as f32 / scale_factor,
            };
            if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
                host.was_resized();
            }
            pane.last_applied_size = Some(size_px);
        }
    }
}

/// Converts a pane's `PaneRectFraction` (0..1 of the whole window's content
/// area) into a physical-pixel `(x, y, width, height)` rect within
/// `container_size`, clamped to stay inside it. `None` for a degenerate
/// (zero or negative) result. Shared between `sync_pane_sizes` above and
/// render.rs's own per-pane viewport computation -- small, intentional
/// duplication of the same handful of lines rather than a cross-module
/// dependency between "what size should CEF think this pane is" and "what
/// rect should wgpu draw this pane's texture into," which are related but
/// separately-owned concerns (one drives CEF's layout, the other drives
/// compositing).
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
            webview_widget.connect_button_press_event(move |_widget, event: &gtk::gdk::EventButton| {
                match event.button() {
                    8 | 9 => {
                        let held = {
                            let mut state = press_state.borrow_mut();
                            *state |= if event.button() == 8 { 0b01 } else { 0b10 };
                            *state
                        };
                        webview.run_javascript(
                            &mouse_backforward_mousedown_js(event, held),
                            None::<&gtk::gio::Cancellable>,
                            |_| {},
                        );
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
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
        let manager = Rc::new(RefCell::new(PaneManager {
            panes: IndexMap::new(),
            open_pane_count: Arc::new(AtomicUsize::new(0)),
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
                // pass-through on it directly. gtk_gl_area_realize()
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
                            event_window.set_pass_through(true);
                            log::info!(
                                "tier3_pane::pane_host: GLArea private event_window \
                                 found and pass-throughed, is_pass_through={} \
                                 (parent GdkWindow is_pass_through={})",
                                event_window.is_pass_through(),
                                window.is_pass_through(),
                            );
                        }
                        [] => log::warn!(
                            "tier3_pane::pane_host: GLArea's parent_window has no \
                             INPUT_ONLY child at realize -- expected \
                             priv->event_window (see gtk_gl_area_realize upstream) \
                             was not found; pass-through fix did not apply"
                        ),
                        multiple => log::error!(
                            "tier3_pane::pane_host: GLArea's parent_window has \
                             {} INPUT_ONLY children at realize -- expected exactly \
                             one (priv->event_window). Refusing to guess which one \
                             is the real event-catching window; pass-through fix did \
                             NOT apply to any of them. This means GTK's own \
                             gtk_gl_area_realize() behavior has changed from what \
                             items.id=227 verified against (GTK 3.24) -- investigate \
                             before trusting click-through on this widget.",
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
                let (width, height) = (width.max(1) as u32, height.max(1) as u32);
                if let Some(rs) = render_state.borrow_mut().as_mut() {
                    rs.resize((width, height));
                }
                let scale = area.scale_factor().max(1) as f32;
                let layout = app_handle
                    .state::<PaneLayoutState>()
                    .0
                    .lock()
                    .unwrap()
                    .clone();
                manager
                    .borrow_mut()
                    .sync_pane_sizes((width, height), scale, &layout);
            });
        }

        {
            let render_state = render_state.clone();
            let gl_context = gl_context.clone();
            let manager = manager.clone();
            let app_handle = app_handle.clone();
            glarea.connect_render(move |_area, _gtk_gl_context| {
                use glow::HasContext as _;
                log::debug!(
                    "DIAG items.id=227: connect_render fired, open_panes={}",
                    manager.borrow().panes.len()
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

                manager.borrow_mut().drain_ready_browsers();

                for pane in manager.borrow().panes.values() {
                    if let Some(host) = pane.browser_lifecycle.browser().and_then(|b| b.host()) {
                        host.send_external_begin_frame();
                    }
                }

                if let Some(rs) = render_state.borrow_mut().as_mut() {
                    let layout = app_handle
                        .state::<PaneLayoutState>()
                        .0
                        .lock()
                        .unwrap()
                        .clone();
                    rs.render(&layout);
                }

                if let (Some(gl), Some(fbo)) = (gl_context.borrow().as_ref(), captured_fbo) {
                    let framebuffer =
                        std::num::NonZeroU32::new(fbo as u32).map(glow::NativeFramebuffer);
                    unsafe {
                        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, framebuffer);
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
                self.manager
                    .borrow_mut()
                    .open_pane(key, url, &device, &queue, scale, size);
                log::debug!("DIAG items.id=227: dispatch(Open) -> glarea.queue_draw()");
                self.glarea.queue_draw();
            }
            PaneCommand::Close { key } => {
                self.manager.borrow_mut().close_pane(&key);
                log::debug!("DIAG items.id=227: dispatch(Close) -> glarea.queue_draw()");
                self.glarea.queue_draw();
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
        let width = self.glarea.allocated_width().max(1) as f32 / scale;
        let height = self.glarea.allocated_height().max(1) as f32 / scale;
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
    /// intent as the old design's immediate `sync_tx` push.
    pub fn queue_draw(&self) {
        log::debug!("DIAG items.id=227: PaneHost::queue_draw() called");
        self.glarea.queue_draw();
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

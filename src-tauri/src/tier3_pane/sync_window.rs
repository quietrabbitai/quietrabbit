//! Manually-synced CEF pane window.
//!
//! Architecture decided 2026-08-01 (see mod.rs docs): no supported path
//! exists to composite CEF's OSR output into Tauri's own webview surface,
//! and native OS-level child-window reparenting is unavailable on Wayland.
//! This module owns a separate `winit` window, manually kept in sync with
//! the main Tauri window's position/size via events forwarded from the
//! Tauri side (see `PaneHandle::sync_to`).
//!
//! Phase A scope: single pane, always visible, no compaction. The pane
//! window is a plain borderless-adjacent window for now (not yet visually
//! docked/styled) -- this phase proves the mechanism, not the final look.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use cef::{ImplBrowser, ImplBrowserHost};

use crate::tier3_pane::render::{ClientBuilder, PaneRenderHandler, RenderState};

/// Owns the pane's winit window, wgpu render state, and the CEF browser
/// instance backing it. Constructed once at startup (Phase A: eagerly, no
/// user-initiated open/close yet -- that's Phase B/C's selector-screen
/// wiring, per TIER3_ACCESS_MODEL.md States 3-4).
pub struct PaneWindow {
    event_loop: Option<EventLoop<()>>,
    app: PaneApp,
}

struct PaneApp {
    window: Option<Arc<Window>>,
    render_state: Option<RenderState>,
    browser: Option<cef::Browser>,
    browser_created: bool,
    pending_render_handler: Option<(PaneRenderHandler, Rc<RefCell<winit::dpi::LogicalSize<f32>>>)>,
    browser_size: Option<Rc<RefCell<winit::dpi::LogicalSize<f32>>>>,
    window_info_and_settings: Option<(cef::WindowInfo, cef::BrowserSettings)>,
    /// Receives the constructed `Browser` from `LifeSpanHandler::on_after_created`
    /// (see render.rs docs) -- CEF's UI thread delivers it here since
    /// creation is now async under multi_threaded_message_loop. Drained in
    /// `PaneWindow::pump()`, same as `window_event` handles other
    /// cross-boundary events.
    browser_ready_rx: Option<std::sync::mpsc::Receiver<cef::Browser>>,
    /// The physical size `sync_to()` last explicitly requested via
    /// `request_inner_size`, shared with `PaneWindow::sync_to` (which runs
    /// outside this struct's own methods, called directly on `self.window`).
    /// Compared against actual `WindowEvent::Resized` sizes to detect
    /// external interference (user maximize/tile/manual-resize) -- see
    /// `last_requested_size_mismatch` doc for why this exists.
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
    /// URL loaded on first browser creation. Phase A: a fixed placeholder
    /// (a real provider URL), not yet wired to the selector screen's chosen
    /// provider -- that wiring is Phase B/C scope (TIER3_ACCESS_MODEL.md
    /// States 3-4, decisions.id=681).
    initial_url: String,
}

/// How long to wait after the last `Resized` event before actually
/// reconfiguring the wgpu surface and notifying CEF. 100ms is comfortably
/// longer than a single frame at any reasonable refresh rate, so it won't
/// visibly delay a genuine one-shot resize, but long enough to coalesce a
/// rapid-fire sequence from a compositor animation into one final apply.
const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

impl PaneWindow {
    /// Creates the pane's winit event loop and window shell. Does not yet
    /// create the CEF browser instance -- that happens on first
    /// `resumed()`/`RedrawRequested`, mirroring the spike's proven pattern
    /// (browser creation deferred to inside the event loop, not attempted
    /// during construction).
    pub fn new(initial_url: impl Into<String>) -> Self {
        let event_loop = EventLoop::new().expect("tier3_pane::sync_window: failed to create winit event loop");
        Self {
            event_loop: Some(event_loop),
            app: PaneApp {
                window: None,
                render_state: None,
                browser: None,
                browser_created: false,
                pending_render_handler: None,
                browser_size: None,
                window_info_and_settings: None,
                browser_ready_rx: None,
                last_requested_size: Rc::new(RefCell::new(None)),
                pending_resize: None,
                last_resize_event_at: None,
                initial_url: initial_url.into(),
            },
        }
    }

    /// Runs one non-blocking iteration of the pane's winit event loop.
    /// Called from Tauri's own `RunEvent::MainEventsCleared` (see mod.rs
    /// docs on the interleaving architecture).
    ///
    /// SIMPLIFIED 2026-08-01 after switching to
    /// `multi_threaded_message_loop = true` (see bootstrap.rs's root-cause
    /// comment): CEF now drives its own message pump entirely on its own
    /// OS thread. `do_message_loop_work()` and the `pending_work` flag
    /// this function used to check are no longer relevant -- CEF's docs
    /// are explicit that neither `CefDoMessageLoopWork()` nor
    /// `CefRunMessageLoop()` need to be called in this mode. This
    /// function's only remaining job is pumping the pane's own `winit`
    /// window/render loop, same as before but without the CEF-pump
    /// bookkeeping.
    ///
    /// The ~60fps throttle sleep is still load-bearing for the same reason
    /// as before (Tauri's `MainEventsCleared` fires unthrottled) --
    /// removing it would still busy-loop this winit pump, independent of
    /// the CEF fix.
    pub fn pump(&mut self) {
        use winit::platform::pump_events::EventLoopExtPumpEvents;

        // Deliver any browser CEF's UI thread finished constructing since
        // the last tick (see render.rs's LifeSpanHandler docs for why this
        // is async now).
        if let Some(rx) = self.app.browser_ready_rx.as_ref() {
            if let Ok(browser) = rx.try_recv() {
                log::info!("tier3_pane::sync_window: browser delivered via on_after_created");
                self.app.browser = Some(browser);
            }
        }

        // Apply a debounced resize once no further Resized events have
        // arrived for RESIZE_DEBOUNCE (see PaneApp::pending_resize docs).
        // Checked here rather than in the Resized handler itself, since
        // pump() is what's actually invoked on a steady ~60fps cadence
        // regardless of how many window events arrive in between.
        if let (Some(size), Some(last_event_at)) =
            (self.app.pending_resize, self.app.last_resize_event_at)
        {
            if last_event_at.elapsed() >= RESIZE_DEBOUNCE {
                self.app.apply_resize(size);
                self.app.pending_resize = None;
                self.app.last_resize_event_at = None;
            }
        }

        if let Some(window) = self.app.window.as_ref() {
            window.request_redraw();
        }

        if let Some(event_loop) = self.event_loop.as_mut() {
            let _ = event_loop.pump_app_events(Some(std::time::Duration::ZERO), &mut self.app);
        }

        std::thread::sleep(std::time::Duration::from_millis(1000 / 60));
    }

    /// Repositions/resizes the pane window to sit flush against the given
    /// rect (in physical pixels), expressed relative to the same screen
    /// coordinate space the main Tauri window reports. Called from Tauri's
    /// window move/resize event handlers -- see mod.rs docs on why this is
    /// manual rather than OS-level child-window reparenting.
    ///
    /// Phase A: places the pane immediately to the right of the given rect,
    /// same height, a fixed placeholder width -- not yet the real
    /// split-screen layout math (States 4-5's actual proportions are
    /// Phase C scope, reusing IA Section 3c's resting/active ratio).
    ///
    /// FOUND THE HARD WAY (2026-08-01): calling this unconditionally on
    /// every tick fights the window manager whenever the user manually
    /// maximizes/tiles/resizes the pane window directly (it has normal
    /// decorations -- min/max/close). `set_outer_position`/
    /// `request_inner_size` both "automatically un-maximize the window if
    /// it's maximized" per winit's own docs -- so a forced sync_to() call
    /// arriving on the very next ~16ms tick after the compositor maximizes
    /// the window immediately un-maximizes it again, and the two fight
    /// continuously. Confirmed directly: KWin reported the pane's geometry
    /// as fractional/non-integer (2327.27...x1277.27...) while "frozen,"
    /// consistent with the window never settling into either state.
    ///
    /// `is_maximized()` is not implemented on Wayland/X11 (per winit's own
    /// docs), so maximize state can't be queried directly. Instead: skip
    /// the forced sync whenever the pane's actual size doesn't match what
    /// `sync_to()` itself last requested -- a mismatch means something
    /// else (the user, the compositor) changed it since, and forcing our
    /// own geometry back is exactly the behavior that caused the freeze.
    /// Once the size matches again (e.g. the user un-maximizes/restores
    /// it), syncing resumes automatically on the next call.
    pub fn sync_to(&self, main_window_rect: PhysicalRect) {
        let Some(window) = self.app.window.as_ref() else {
            return;
        };

        let actual_size = window.inner_size();
        let expected = *self.app.last_requested_size.borrow();
        if let Some(expected) = expected {
            if actual_size != expected {
                // Something external changed the pane's size since we last
                // set it -- back off rather than fight it.
                return;
            }
        }

        const PLACEHOLDER_PANE_WIDTH: i32 = 480;
        let pane_x = main_window_rect.x + main_window_rect.width;
        window.set_outer_position(winit::dpi::PhysicalPosition::new(
            pane_x,
            main_window_rect.y,
        ));
        let new_size = winit::dpi::PhysicalSize::new(
            PLACEHOLDER_PANE_WIDTH as u32,
            main_window_rect.height as u32,
        );
        let _ = window.request_inner_size(new_size);
        *self.app.last_requested_size.borrow_mut() = Some(new_size);
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

impl ApplicationHandler for PaneApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("Quiet Rabbit — Tier 3 pane (Phase A)"))
                .expect("tier3_pane::sync_window: failed to create pane window"),
        );

        let render_state = pollster::block_on(RenderState::new(window.clone()));

        // accelerated_osr is always enabled for the `cef` dependency (see
        // Cargo.toml) -- the platform check alone determines whether the
        // shared-texture path is used vs. the software on_paint fallback.
        let accelerated_osr = cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux"));
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
        );

        self.window = Some(window.clone());
        self.render_state = Some(render_state);
        self.pending_render_handler = Some((render_handler, browser_size.clone()));
        self.browser_size = Some(browser_size);
        self.window_info_and_settings = Some((window_info, browser_settings));

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Phase A: closing the pane window does not exit the whole
                // app (Tauri's main window owns that decision). Just stop
                // this event loop's own iteration; PaneWindow::pump becomes
                // a no-op once event_loop is exhausted.
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Some(host) = self.browser.as_mut().and_then(|b| b.host()) {
                    host.send_external_begin_frame();
                }
                if let (Some(render_state), Some(window)) =
                    (self.render_state.as_mut(), self.window.as_ref())
                {
                    render_state.render(window);
                    // FOUND THE HARD WAY (2026-08-01): do NOT call
                    // window.request_redraw() unconditionally here. Unlike
                    // the spike's bare winit main() loop -- which pairs
                    // this exact pattern with an outer thread::sleep
                    // directly adjacent in the same synchronous loop --
                    // this pump() is invoked externally, once per Tauri
                    // MainEventsCleared tick, via
                    // event_loop.pump_app_events(Duration::ZERO, ...). A
                    // self-requested redraw re-queues immediately and a
                    // zero-timeout pump drains the queue to exhaustion
                    // before returning control, which starves pump()'s own
                    // throttling sleep and pins the main thread at ~100%
                    // CPU (confirmed via top/ps). send_external_begin_frame
                    // above already drives CEF's own windowless_frame_rate
                    // cadence (60fps, see resumed()); this handler redraws
                    // whatever CEF_TEXTURE currently holds once per pump
                    // tick, which pump()'s own ~60fps sleep already paces
                    // correctly -- no self-perpetuating redraw needed.
                }

                if !self.browser_created {
                    self.browser_created = true;
                    if let (Some((render_handler, _)), Some((window_info, browser_settings))) = (
                        self.pending_render_handler.take(),
                        self.window_info_and_settings.as_ref(),
                    ) {
                        let url = cef::CefString::from(self.initial_url.as_str());
                        // ASYNC creation (2026-08-01): browser_host_create_browser_sync
                        // requires being called from CEF's own UI thread, which is no
                        // longer this thread under multi_threaded_message_loop (see
                        // render.rs's LifeSpanHandler docs -- found the hard way, real
                        // run returned browser_host_create_browser_sync -> false with
                        // no error). browser_host_create_browser is callable from any
                        // thread and delivers the Browser asynchronously via
                        // on_after_created, received in PaneWindow::pump().
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.browser_ready_rx = Some(rx);
                        let created = cef::browser_host_create_browser(
                            Some(window_info),
                            Some(&mut ClientBuilder::build(render_handler, tx)),
                            Some(&url),
                            Some(browser_settings),
                            None,
                            None, // default in-memory context -- Phase A scope; real
                                  // per-provider isolated contexts are Phase B.
                        );
                        log::info!(
                            "tier3_pane::sync_window: browser_host_create_browser (async) dispatched -> {created}"
                        );
                    }
                }
            }
            WindowEvent::Resized(size) => {
                // Debounced (see PaneApp::pending_resize / PaneWindow::pump
                // docs) -- record only, don't act yet. Acting synchronously
                // here on every intermediate event during a compositor
                // resize/maximize gesture is what stalled the pane's event
                // loop (found the hard way, 2026-08-01).
                self.pending_resize = Some(size);
                self.last_resize_event_at = Some(std::time::Instant::now());
            }
            _ => {}
        }
    }
}

impl PaneApp {
    /// Actually applies a settled resize: reconfigures the wgpu surface
    /// and notifies CEF's browser host. Called from `PaneWindow::pump()`
    /// once `RESIZE_DEBOUNCE` has elapsed since the last raw `Resized`
    /// event -- see that method and `pending_resize`'s docs for why this
    /// is deferred rather than applied directly in `window_event`.
    fn apply_resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if let Some(render_state) = self.render_state.as_mut() {
            render_state.resize(size);
        }
        if let (Some(browser_size), Some(window)) = (self.browser_size.as_ref(), self.window.as_ref())
        {
            *browser_size.borrow_mut() = size.to_logical(window.scale_factor());
            if let Some(host) = self.browser.as_mut().and_then(|b| b.host()) {
                host.was_resized();
            }
        }
    }
}

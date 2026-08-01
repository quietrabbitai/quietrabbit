//! CEF process bootstrap: subprocess dispatch and initialization.
//!
//! # Why this exists as a separate, first-called module
//! CEF's multi-process architecture spawns renderer/GPU/utility helper
//! processes by re-executing the same binary with a `--type=...` flag.
//! `cef::execute_process` is how the CEF-Rust binding intercepts those
//! re-invocations: for a helper-process invocation it runs CEF's own
//! subprocess entry point and the calling process should exit immediately
//! after; for the real browser-process invocation it returns and normal
//! startup continues.
//!
//! This MUST run before Tokio (or any other runtime/threading machinery)
//! initializes anything in the process. Per Tokio's own documentation
//! (tokio::runtime module docs): forking without an immediate exec is only
//! supported "before the parent process has used Tokio in any way" -- and
//! CEF's subprocess spawn is exactly this kind of fork+exec on Linux.
//! `#[tokio::main] async fn main()` enters the Tokio runtime as the first
//! thing that happens in the function body, before any other code runs --
//! incompatible with this requirement. This is why `main.rs` was restructured
//! (2026-08-01) to a plain `fn main()` that calls
//! `dispatch_cef_subprocess()` first and only then constructs and enters a
//! Tokio runtime manually for the browser-process path.
//!
//! Reuses the proven config from the retained spike
//! (qr-spike-192/cef-rs/examples/osr_popup_views_test/src/main.rs and
//! webrender.rs), items.id=192-201: the SoftNavigationDetection crash
//! workaround, `root_cache_path` for persistent RequestContext isolation,
//! `external_message_pump` for host-loop interleaving (see mod.rs docs).

use cef::{self, args::Args, *};

/// Runs CEF's own subprocess entry point if this invocation of the QR
/// binary is a CEF-spawned helper process (renderer, GPU, utility, etc).
///
/// Returns `true` if this call handled a subprocess invocation and the
/// caller (`main.rs`) must return immediately without doing anything else
/// -- no Tokio runtime, no Tauri, nothing. Returns `false` if this is the
/// real browser-process invocation and normal startup should continue.
///
/// Must be the first thing called in `main()`, before any other code --
/// see module docs for why.
pub fn dispatch_cef_subprocess() -> bool {
    // MUST be the first CEF API call in the process -- every proven spike
    // example (qr-spike-192/cef-rs/examples/*) calls this before Args::new().
    // Found the hard way, 2026-08-01: omitting it causes an immediate abort
    // inside cef_command_line_create() (Chromium's IMMEDIATE_CRASH(),
    // confirmed via coredumpctl backtrace, not a segfault/memory bug) --
    // CEF enforces that API version negotiation happens before any other
    // CEF API touch, including Args::new()/as_cmd_line().
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let cmd = args.as_cmd_line().expect(
        "tier3_pane::bootstrap: CEF could not parse this process's command line -- \
         cannot safely determine subprocess vs. browser-process invocation",
    );

    let type_switch = cef::CefString::from("type");
    let is_browser_process = cmd.has_switch(Some(&type_switch)) != 1;

    let mut app = AppBuilder::build(Tier3PaneApp::new());
    let ret = cef::execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());

    if is_browser_process {
        // Per every CEF-Rust example in the retained spike: execute_process
        // returns -1 for the actual browser-process invocation (it did not
        // dispatch to a subprocess entry point because there was none to
        // dispatch to). Any other return value here means CEF is confused
        // about which process this is -- a startup-time invariant violation,
        // not a recoverable runtime condition.
        assert_eq!(
            ret, -1,
            "tier3_pane::bootstrap: execute_process returned {ret} for what CEF's own \
             command-line parse identified as the browser process -- expected -1"
        );
        false
    } else {
        log::info!("tier3_pane::bootstrap: dispatched CEF subprocess, exit_code={ret}");
        true
    }
}

/// Root cache path for CEF's global settings (`Settings.root_cache_path`).
///
/// Required in addition to per-RequestContext `cache_path` -- items.id=192's
/// finding: without this, CEF emits a startup warning and per-context
/// persistence silently degrades to in-memory (`flush_store` reports success
/// but nothing persists). This is the QR-specific path, distinct from the
/// spike's throwaway `/tmp/qr_*` paths.
///
/// TODO (Phase B, when real provider RequestContexts are wired): resolve
/// this from Tauri's app_data_dir rather than a fixed path, so it lives
/// under QR's normal per-user data directory rather than a hardcoded
/// location. Fixed for Phase A since only bootstrap/render verification is
/// in scope, not real per-provider persistence.
pub const ROOT_CACHE_PATH: &str = "/tmp/qr_tier3_pane_root_cache";

/// Initializes CEF for the browser process. Must be called exactly once,
/// after `dispatch_cef_subprocess()` has returned `false` (i.e. this is
/// confirmed to be the browser-process invocation), and before any browser
/// instance is created.
///
/// Returns the shared flag CEF's `OnScheduleMessagePumpWork` callback
/// writes to -- the host loop (Tauri's `RunEvent::MainEventsCleared`, see
/// mod.rs) polls this each iteration to decide whether to call
/// `cef::do_message_loop_work()`. Mirrors items.id=200's fix: a `Mutex`
/// (not `RefCell`/`Rc`) because CEF documents this callback as callable
/// "on any thread" -- confirmed the hard way in the spike (RefCell panicked
/// with "already borrowed" under real cross-thread contention).
pub fn initialize_cef() -> CefInitResult {
    initialize_cef_with_pump_setting(true)
}

/// Result of `initialize_cef()`. `pending_work` is retained for
/// compatibility with the (now bypassed in multi_threaded_message_loop
/// mode) OnScheduleMessagePumpWork callback -- see module docs. Callers
/// running in multi_threaded_message_loop mode do not need to read it.
pub struct CefInitResult {
    pub pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>,
}

/// Diagnostic variant, retained from the (failed) GMainContext-isolation
/// experiment for the external_message_pump comparison -- the
/// `external_message_pump` parameter is now IGNORED when
/// `multi_threaded_message_loop` is used (see module docs), kept only so
/// bin/bare_tauri_cpu_test.rs still compiles against this signature.
/// Production code should call `initialize_cef()`.
pub fn initialize_cef_with_pump_setting(_external_message_pump_ignored: bool) -> CefInitResult {
    // ROOT CAUSE + FIX HISTORY (2026-08-01, items.id=3 Phase A) --
    // read this before touching this function again.
    //
    // SYMPTOM: cef::initialize() alone (no browser, no pump call) pins the
    // main thread at 40-90%+ CPU. Confirmed via strace -k: call stack is
    // gtk_main_iteration_do -> g_main_context_iteration -> ppoll with a
    // zero timeout, ~37k calls/sec, on Tauri's own main/GTK thread.
    //
    // RULED OUT:
    //  - Our own pump()/render loop: instrumented, correctly throttled,
    //    sub-ms per call -- the busy-poll persists through multi-second
    //    gaps where pump() makes zero calls.
    //  - external_message_pump setting: tested true vs false, identical
    //    CPU behavior either way.
    //  - Pre-existing Tauri/GTK issue independent of CEF: bare-Tauri
    //    isolation test (no CEF code at all) sits under 1% CPU, sustained.
    //  - GMainContext thread-default isolation (glib::MainContext::new()
    //    + with_thread_default() wrapped around cef::initialize()):
    //    IMPLEMENTED AND TESTED, NO EFFECT. Identical call stack/CPU
    //    after the fix as before it. Root cause: per CEF's own Linux
    //    issue tracker (chromiumembedded/cef#2512, #3087), Chromium's
    //    MessagePumpGlib does not respect the embedder's thread-default
    //    context -- as of Chromium M86 it creates and manages its own
    //    GMainContext internally. Pushing a thread-default context before
    //    cef::initialize() has no effect on where CEF's GSource attaches,
    //    which is why this fix measured zero change.
    //
    // ACTUAL FIX: CefSettings.multi_threaded_message_loop = true. Per
    // CEF's own docs (chromiumembedded.github.io/cef/general_usage.html):
    // "This will cause CEF to run the browser UI thread on a separate
    // thread from the main application thread. With this approach neither
    // CefDoMessageLoopWork() nor CefRunMessageLoop() need to be called."
    // This is a structural fix, not a workaround: GTK's loop only ever
    // iterates sources on the thread it's running on (Tauri's main
    // thread); if CEF's UI thread is a genuinely different OS thread, GTK
    // never touches CEF's GLib sources regardless of which GMainContext
    // they're attached to -- no context-object trickery needed.
    //
    // Historical GTK2-era docs (cef#2512, 2018) describe extra manual
    // steps for this mode on Linux (XInitThreads, gdk_threads_init, a
    // hand-rolled GMainContext/GMainLoop pair, gdk_threads_enter/leave
    // around GTK calls). These are OBSOLETE for our case: cef#3087 (2021,
    // Chromium M86+) shows CEF's own GTK-side reference implementation
    // was simplified to just use the default GLib context once CEF
    // started managing its own context internally -- and separately, we
    // never touch GTK/GDK window handles for the CEF pane at all (it's
    // off-screen/windowless rendering, per decisions.id=699 Option 3b),
    // which is precisely the class of concern (GDK thread-safety, X11
    // window handle creation from a non-GTK thread) those old docs were
    // guarding against. Not yet verified whether any GDK-thread-safety
    // step is still needed for us specifically -- flagged as a thing to
    // watch for in testing, not assumed safe by this reasoning alone.
    //
    // pending_work / on_schedule_message_pump_work are effectively
    // unused in this mode (CEF drives its own loop internally and never
    // calls that callback when multi_threaded_message_loop is set), but
    // left wired for now rather than torn out, in case testing reveals
    // multi_threaded_message_loop doesn't fully eliminate the need for
    // it on this CEF version.
    let args = Args::new();
    let pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    let mut app = AppBuilder::build(Tier3PaneApp::new_with_pump(pending_work.clone()));

    let settings = Settings {
        windowless_rendering_enabled: true as _,
        multi_threaded_message_loop: 1,
        no_sandbox: 1,
        root_cache_path: cef::CefString::from(ROOT_CACHE_PATH).into(),
        ..Default::default()
    };

    let init_ret = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    assert_eq!(
        init_ret, 1,
        "tier3_pane::bootstrap: cef::initialize failed (returned {init_ret})"
    );

    log::info!(
        "tier3_pane::bootstrap: CEF initialized, root_cache_path={ROOT_CACHE_PATH}, multi_threaded_message_loop=true"
    );
    CefInitResult { pending_work }
}

// --- App / command-line handling ---
//
// Two construction paths (`new` / `new_with_pump`) because
// dispatch_cef_subprocess() runs before the pump-scheduling flag exists
// (it's only needed by the browser process, not helper-process dispatch)
// -- avoids threading an Option through the subprocess-dispatch hot path
// for a value that's always None there.

#[derive(Clone)]
pub struct Tier3PaneApp {
    pending_work: Option<std::sync::Arc<std::sync::Mutex<Option<i64>>>>,
}

impl Tier3PaneApp {
    fn new() -> Self {
        Self { pending_work: None }
    }

    fn new_with_pump(pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>) -> Self {
        Self {
            pending_work: Some(pending_work),
        }
    }
}

wrap_app! {
    pub(crate) struct AppBuilder {
        app: Tier3PaneApp,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&cef::CefStringUtf16>,
            command_line: Option<&mut cef::CommandLine>,
        ) {
            let Some(command_line) = command_line else { return; };

            // Crash workaround proven in items.id=192/195/196/198/199/200/201:
            // ReadAnythingSoftNavigationObserver::OnSoftNavigation() calls
            // tabs::TabInterface::GetFromContents(), null in CEF's tabless
            // model, SIGSEGV on any SPA soft navigation -- every provider QR
            // intends to embed is an SPA. Merge into any existing
            // disable-features value rather than overwrite, since Chromium's
            // CommandLine takes the last value for a switch.
            let df_switch = cef::CefString::from("disable-features");
            let existing = if command_line.has_switch(Some(&df_switch)) != 0 {
                let v: cef::CefString = (&command_line.switch_value(Some(&df_switch))).into();
                v.to_string()
            } else {
                String::new()
            };
            let merged = if existing.is_empty() {
                "SoftNavigationDetection".to_string()
            } else if existing.split(',').any(|f| f == "SoftNavigationDetection") {
                existing
            } else {
                format!("{existing},SoftNavigationDetection")
            };
            command_line.append_switch_with_value(Some(&df_switch), Some(&merged.as_str().into()));
        }

        fn browser_process_handler(&self) -> Option<cef::BrowserProcessHandler> {
            self.app.pending_work.clone().map(|pending_work| {
                BrowserProcessHandlerBuilder::build(Tier3PaneBrowserProcessHandler::new(pending_work))
            })
        }
    }
}

impl AppBuilder {
    pub(crate) fn build(app: Tier3PaneApp) -> cef::App {
        Self::new(app)
    }
}

#[derive(Clone)]
pub struct Tier3PaneBrowserProcessHandler {
    pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>,
}

impl Tier3PaneBrowserProcessHandler {
    fn new(pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>) -> Self {
        Self { pending_work }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct BrowserProcessHandlerBuilder {
        handler: Tier3PaneBrowserProcessHandler,
    }

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            // Called "on any thread" per CEF's own doc comment -- Mutex, not
            // RefCell/Rc (items.id=200's finding, confirmed the hard way).
            let mut pending = self.handler.pending_work.lock().unwrap();
            *pending = Some(delay_ms);
        }
    }
}

impl BrowserProcessHandlerBuilder {
    pub(crate) fn build(handler: Tier3PaneBrowserProcessHandler) -> BrowserProcessHandler {
        Self::new(handler)
    }
}

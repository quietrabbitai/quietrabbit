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
        "cloud_chat_gpu_pane::bootstrap: CEF could not parse this process's command line -- \
         cannot safely determine subprocess vs. browser-process invocation",
    );

    let type_switch = cef::CefString::from("type");
    let is_browser_process = cmd.has_switch(Some(&type_switch)) != 1;

    let mut app = AppBuilder::build(CloudChatGpuPaneApp::new());
    let ret = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if is_browser_process {
        // Per every CEF-Rust example in the retained spike: execute_process
        // returns -1 for the actual browser-process invocation (it did not
        // dispatch to a subprocess entry point because there was none to
        // dispatch to). Any other return value here means CEF is confused
        // about which process this is -- a startup-time invariant violation,
        // not a recoverable runtime condition.
        assert_eq!(
            ret, -1,
            "cloud_chat_gpu_pane::bootstrap: execute_process returned {ret} for what CEF's own \
             command-line parse identified as the browser process -- expected -1"
        );
        false
    } else {
        log::info!("cloud_chat_gpu_pane::bootstrap: dispatched CEF subprocess, exit_code={ret}");
        true
    }
}

/// Initializes CEF for the browser process. Must be called exactly once,
/// after `dispatch_cef_subprocess()` has returned `false` (i.e. this is
/// confirmed to be the browser-process invocation), and before any browser
/// instance is created.
///
/// `root_cache_path` is CEF's global `Settings.root_cache_path`, required in
/// addition to each pane's own per-provider `RequestContextSettings.cache_path`
/// (items.id=192's finding: without this, CEF emits a startup warning and
/// per-context persistence silently degrades to in-memory -- `flush_store`
/// reports success but nothing persists). Phase A used a fixed
/// `/tmp/qr_tier3_pane_root_cache` path (bootstrap/render verification only,
/// no real per-provider persistence yet); Phase B (items.id=202 piece 5)
/// resolves this from Tauri's `app_data_dir` instead, per this const's own
/// prior TODO -- the caller (main.rs) is responsible for that resolution,
/// this function just takes the result.
///
/// Returns the shared flag CEF's `OnScheduleMessagePumpWork` callback
/// writes to -- the host loop (Tauri's `RunEvent::MainEventsCleared`, see
/// mod.rs) polls this each iteration to decide whether to call
/// `cef::do_message_loop_work()`. Mirrors items.id=200's fix: a `Mutex`
/// (not `RefCell`/`Rc`) because CEF documents this callback as callable
/// "on any thread" -- confirmed the hard way in the spike (RefCell panicked
/// with "already borrowed" under real cross-thread contention).
///
/// Returns `Err` when `cef::initialize` reports failure (items.id=646), e.g.
/// the cache directory is held by another process. The caller must then treat
/// CEF as unavailable: `cef_available()` stays false, and neither
/// `cef::shutdown()` nor any other CEF call is safe.
pub fn initialize_cef(root_cache_path: &std::path::Path) -> Result<CefInitResult, CefInitError> {
    initialize_cef_with_pump_setting(root_cache_path, true)
}

/// Why `initialize_cef()` failed.
#[derive(Debug, thiserror::Error)]
pub enum CefInitError {
    #[error("cef::initialize failed (returned {0})")]
    InitializeFailed(i32),
}

/// True only after `initialize_cef*` returned `Ok` in this process, so a
/// failed or never-run init reads as unavailable. Process-wide, like
/// CEF itself.
static CEF_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether CEF initialised in this process (items.id=646). Gate every CEF
/// touch point on this: the Cloud Chat pane commands, the pane host install,
/// and `cef::shutdown()`.
pub fn cef_available() -> bool {
    CEF_AVAILABLE.load(std::sync::atomic::Ordering::Acquire)
}

/// Returned by the Cloud Chat pane commands when CEF is unavailable. Plain
/// text for the UI: no internals, no paths (those are in the log).
pub const CEF_UNAVAILABLE_MESSAGE: &str = "Cloud Chat is unavailable: the embedded browser failed \
to start. Restart Quiet Rabbit; if it keeps happening, check the log.";

/// `Ok(())` when CEF initialised, otherwise `CEF_UNAVAILABLE_MESSAGE`.
pub fn ensure_cef_available() -> Result<(), String> {
    if cef_available() {
        Ok(())
    } else {
        Err(CEF_UNAVAILABLE_MESSAGE.to_string())
    }
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
pub fn initialize_cef_with_pump_setting(
    root_cache_path: &std::path::Path,
    _external_message_pump_ignored: bool,
) -> Result<CefInitResult, CefInitError> {
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

    let mut app = AppBuilder::build(CloudChatGpuPaneApp::new_with_pump(pending_work.clone()));

    let root_cache_path_str = root_cache_path.to_string_lossy().into_owned();
    let settings = Settings {
        windowless_rendering_enabled: true as _,
        multi_threaded_message_loop: 1,
        no_sandbox: 1,
        root_cache_path: cef::CefString::from(root_cache_path_str.as_str()),
        ..Default::default()
    };

    let init_ret = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );

    if init_ret != 1 {
        log::error!(
            "cloud_chat_gpu_pane::bootstrap: cef::initialize failed (returned {init_ret}), \
             root_cache_path={root_cache_path_str} -- Cloud Chat is unavailable this run"
        );
        return Err(CefInitError::InitializeFailed(init_ret));
    }

    CEF_AVAILABLE.store(true, std::sync::atomic::Ordering::Release);
    log::info!(
        "cloud_chat_gpu_pane::bootstrap: CEF initialized, root_cache_path={root_cache_path_str}, multi_threaded_message_loop=true"
    );
    Ok(CefInitResult { pending_work })
}

// --- App / command-line handling ---

/// Chromium switches that stop the browser's own background network traffic
/// (items.id=622). A net-log of an idle CEF 151 with none of these showed
/// calls to clients2.google.com and update.googleapis.com (component
/// updater, the latter also fetching from edgedl.me.gvt1.com), plus
/// accounts.google.com, safebrowsing.googleapis.com and the
/// www.google.com AI Mode eligibility check, with no pane open and nobody
/// signed in. Page-driven traffic from a provider's own site is not covered
/// and is not meant to be.
///
/// Known remainder: one POST to accounts.google.com/ListAccounts at startup
/// (the browser profile's Google account cookie check). Nothing tried removes
/// it (disable-sync, signin.allowed=false in a seeded profile, a seeded
/// gaia_cookie report time, --incognito, AvoidAutoTriggerListAccountsOnStale,
/// several feature names), and it does not repeat while idle. Revisit with the
/// profile replacement in items.id=622 part 2b.
const BACKGROUND_NETWORK_SWITCHES: &[&str] = &[
    "disable-background-networking",
    "disable-component-update",
    "disable-domain-reliability",
    "no-pings",
    "disable-sync",
    "metrics-recording-only",
    "disable-default-apps",
];

/// Features turned off through `--disable-features` (names verified against
/// the strings of the libcef 151 binary).
///
/// - SoftNavigationDetection: crash workaround proven in items.id=192/195/196/
///   198/199/200/201. ReadAnythingSoftNavigationObserver::OnSoftNavigation()
///   calls tabs::TabInterface::GetFromContents(), null in CEF's tabless model,
///   SIGSEGV on any SPA soft navigation, and every provider QR intends to
///   embed is an SPA.
/// - AutofillServerCommunication: Autofill's server queries. Any page with a
///   form (a provider's prompt box) makes the browser GET
///   content-autofill.googleapis.com/v1/pages/..., sending Google a hash of
///   the page's form structure. Page-driven, so the background switches above
///   do not cover it.
/// - NetworkTimeServiceQuerying: the network time tracker's GET to
///   clients2.google.com/time/1/current, sent at every startup.
/// - AimEnabled, AimServerRequestOnStartupEnabled,
///   AimServerRequestOnIdentityChangeEnabled: the AI Mode eligibility service
///   (AimEligibilityService) fetches www.google.com/async/folae at every
///   startup, whatever is on screen.
const DISABLED_FEATURES: &[&str] = &[
    "SoftNavigationDetection",
    "AutofillServerCommunication",
    "NetworkTimeServiceQuerying",
    "AimEnabled",
    "AimServerRequestOnStartupEnabled",
    "AimServerRequestOnIdentityChangeEnabled",
];

/// `existing` (a comma-separated `--disable-features` value, possibly empty)
/// with each of `wanted` added once. Existing entries keep their order.
fn merge_disabled_features(existing: &str, wanted: &[&str]) -> String {
    let mut features: Vec<&str> = existing.split(',').filter(|f| !f.is_empty()).collect();
    for name in wanted {
        if !features.contains(name) {
            features.push(name);
        }
    }
    features.join(",")
}

/// Environment variable that turns on the GPU diagnostic logging switches in
/// a debug build (items.id=646). Ignored by release builds.
#[cfg(any(debug_assertions, test))]
const GPU_DIAG_ENV: &str = "QR_CEF_GPU_DIAG";

/// The opt-in is the exact value "1"; anything else, or unset, is off.
#[cfg(any(debug_assertions, test))]
fn gpu_diag_requested(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

//
// Two construction paths (`new` / `new_with_pump`) because
// dispatch_cef_subprocess() runs before the pump-scheduling flag exists
// (it's only needed by the browser process, not helper-process dispatch)
// -- avoids threading an Option through the subprocess-dispatch hot path
// for a value that's always None there.

#[derive(Clone)]
pub struct CloudChatGpuPaneApp {
    pending_work: Option<std::sync::Arc<std::sync::Mutex<Option<i64>>>>,
}

impl CloudChatGpuPaneApp {
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
        app: CloudChatGpuPaneApp,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&cef::CefStringUtf16>,
            command_line: Option<&mut cef::CommandLine>,
        ) {
            let Some(command_line) = command_line else { return; };

            // Merge into any existing disable-features value rather than
            // overwrite, since Chromium's CommandLine takes the last value for
            // a switch. See DISABLED_FEATURES for what is switched off and why.
            let df_switch = cef::CefString::from("disable-features");
            let existing = if command_line.has_switch(Some(&df_switch)) != 0 {
                let v: cef::CefString = (&command_line.switch_value(Some(&df_switch))).into();
                v.to_string()
            } else {
                String::new()
            };
            let merged = merge_disabled_features(&existing, DISABLED_FEATURES);
            command_line.append_switch_with_value(Some(&df_switch), Some(&merged.as_str().into()));

            // items.id=622: no background network traffic from the browser
            // itself. See BACKGROUND_NETWORK_SWITCHES.
            for name in BACKGROUND_NETWORK_SWITCHES {
                command_line.append_switch(Some(&cef::CefString::from(*name)));
            }

            // items.id=327 diagnostic only. Confirms whether CEF's OSR
            // compositor is using hardware WebGL or has fallen back to
            // SwiftShader software rendering, since Cloudflare Turnstile is
            // known to treat the software-fallback case as a bot signal.
            // DEV-ONLY PATH (items.id=646): compiled out of release builds,
            // and in debug builds only active with QR_CEF_GPU_DIAG=1 (the
            // switches are verbose and write to stderr). Removal tracked
            // with items.id=327/649.
            #[cfg(debug_assertions)]
            if gpu_diag_requested(std::env::var_os(GPU_DIAG_ENV).as_deref()) {
                command_line.append_switch_with_value(
                    Some(&cef::CefString::from("enable-logging")),
                    Some(&"stderr".into()),
                );
                command_line.append_switch_with_value(
                    Some(&cef::CefString::from("v")),
                    Some(&"1".into()),
                );
                command_line.append_switch_with_value(
                    Some(&cef::CefString::from("vmodule")),
                    Some(
                        &"*gpu*=2,*angle*=2,*gl_context*=2,*gl_surface*=2,*gl_display*=2".into(),
                    ),
                );
            }
        }

        fn browser_process_handler(&self) -> Option<cef::BrowserProcessHandler> {
            self.app.pending_work.clone().map(|pending_work| {
                BrowserProcessHandlerBuilder::build(CloudChatGpuPaneBrowserProcessHandler::new(pending_work))
            })
        }
    }
}

impl AppBuilder {
    pub(crate) fn build(app: CloudChatGpuPaneApp) -> cef::App {
        Self::new(app)
    }
}

#[derive(Clone)]
pub struct CloudChatGpuPaneBrowserProcessHandler {
    pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>,
}

impl CloudChatGpuPaneBrowserProcessHandler {
    fn new(pending_work: std::sync::Arc<std::sync::Mutex<Option<i64>>>) -> Self {
        Self { pending_work }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct BrowserProcessHandlerBuilder {
        handler: CloudChatGpuPaneBrowserProcessHandler,
    }

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            // Called "on any thread" per CEF's own doc comment -- Mutex, not
            // RefCell/Rc (items.id=200's finding, confirmed the hard way).
            let mut pending = super::lock_recover(&self.handler.pending_work);
            *pending = Some(delay_ms);
        }
    }
}

impl BrowserProcessHandlerBuilder {
    pub(crate) fn build(handler: CloudChatGpuPaneBrowserProcessHandler) -> BrowserProcessHandler {
        Self::new(handler)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn gpu_diag_is_off_unless_exactly_one() {
        assert!(!gpu_diag_requested(None));
        assert!(!gpu_diag_requested(Some(OsStr::new(""))));
        assert!(!gpu_diag_requested(Some(OsStr::new("0"))));
        assert!(!gpu_diag_requested(Some(OsStr::new("true"))));
        assert!(gpu_diag_requested(Some(OsStr::new("1"))));
    }

    #[test]
    fn disabled_features_merge_keeps_existing_and_adds_each_once() {
        assert_eq!(merge_disabled_features("", &["A", "B"]), "A,B");
        assert_eq!(merge_disabled_features("X,A", &["A", "B"]), "X,A,B");
        assert_eq!(merge_disabled_features("X,", &["A"]), "X,A");
        assert_eq!(merge_disabled_features("A,B", &["A", "B"]), "A,B");
        assert_eq!(merge_disabled_features("", &[]), "");
    }

    #[test]
    fn soft_navigation_workaround_stays_in_the_feature_list() {
        assert!(DISABLED_FEATURES.contains(&"SoftNavigationDetection"));
        assert!(merge_disabled_features("", DISABLED_FEATURES).contains("SoftNavigationDetection"));
    }

    #[test]
    fn background_network_switches_are_exactly_the_item_622_set() {
        let mut got: Vec<&str> = BACKGROUND_NETWORK_SWITCHES.to_vec();
        got.sort_unstable();
        let mut want = vec![
            "disable-background-networking",
            "disable-component-update",
            "disable-domain-reliability",
            "disable-default-apps",
            "disable-sync",
            "metrics-recording-only",
            "no-pings",
        ];
        want.sort_unstable();
        assert_eq!(got, want);
        assert!(BACKGROUND_NETWORK_SWITCHES
            .iter()
            .all(|s| !s.starts_with('-') && !s.contains('=')));
    }

    #[test]
    fn cef_is_unavailable_until_initialised() {
        // No test in this binary initialises CEF, so the default must deny.
        assert!(!cef_available());
        assert_eq!(ensure_cef_available().unwrap_err(), CEF_UNAVAILABLE_MESSAGE);
    }
}

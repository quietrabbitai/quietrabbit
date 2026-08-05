// Standalone repro/verification harness for items.id=224 (per-pane
// RequestContext cookie persistence failure -- diagnosed) and its
// resolution (decisions.id=711 -- domain-scoped single cookie jar).
// Bypasses Tauri/the frontend entirely -- drives
// tier3_pane::sync_window::PaneWindow the same way commands/tier3_pane.rs's
// open_tier3_panes/close_tier3_pane do, against a fresh throwaway
// root_cache_path so it never touches the real app_data_dir. Diagnostic
// only, not part of the app.
//
// ORIGINAL FINDING (kept for history): the per-pane contexts/<key>
// RequestContext this harness used to build always failed --
// chrome_browser_context.cc:116 "Cannot create profile at path", confirmed
// via gdb to never reach Chromium's real ProfileManager::CreateProfileAsync
// for any second RequestContext. sync_window.rs no longer builds one at
// all (every pane shares CEF's one global context/cookie jar); this
// harness now verifies THAT design instead: that per-provider cookies set
// directly against the global jar persist across a pane close/reopen, and
// that two simultaneously-open different-provider panes don't cross-
// contaminate each other's cookies (domain scoping inside the one shared
// jar).

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cef::rc::Rc as _;
use cef::{
    wrap_cookie_visitor, wrap_set_cookie_callback, Basetime, CefString, Cookie, CookiePriority,
    CookieSameSite, CookieVisitor, ImplCookieManager, ImplCookieVisitor, ImplSetCookieCallback,
    SetCookieCallback, WrapCookieVisitor, WrapSetCookieCallback,
};
use quietrabbit_lib::tier3_pane::sync_window::{PaneCommand, PaneWindow};

const OP_TIMEOUT: Duration = Duration::from_millis(2000);

wrap_set_cookie_callback! {
    struct SetCookieDone {
        tx: Arc<Mutex<Option<mpsc::Sender<bool>>>>,
    }
    impl SetCookieCallback {
        fn on_complete(&self, success: std::os::raw::c_int) {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                let _ = tx.send(success != 0);
            }
        }
    }
}

wrap_cookie_visitor! {
    struct CollectCookies {
        found: Arc<Mutex<Vec<(String, String)>>>, // (name, value)
        done_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    }
    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            count: std::os::raw::c_int,
            total: std::os::raw::c_int,
            _delete_cookie: Option<&mut std::os::raw::c_int>,
        ) -> std::os::raw::c_int {
            if let Some(cookie) = cookie {
                self.found
                    .lock()
                    .unwrap()
                    .push((cookie.name.to_string(), cookie.value.to_string()));
            }
            if count + 1 >= total {
                if let Some(tx) = self.done_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
            1
        }
    }
}

fn set_cookie_blocking(url: &str, name: &str, value: &str) -> bool {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        println!("FAIL: no global cookie manager");
        return false;
    };
    let cookie = Cookie {
        name: CefString::from(name),
        value: CefString::from(value),
        domain: CefString::from(""),
        path: CefString::from("/"),
        secure: 0,
        httponly: 0,
        creation: Basetime::default(),
        last_access: Basetime::default(),
        has_expires: 0,
        expires: Basetime::default(),
        same_site: CookieSameSite::UNSPECIFIED,
        priority: CookiePriority::MEDIUM,
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel();
    let mut callback = SetCookieDone::new(Arc::new(Mutex::new(Some(tx))));
    let dispatched = manager.set_cookie(
        Some(&CefString::from(url)),
        Some(&cookie),
        Some(&mut callback),
    );
    if dispatched == 0 {
        println!("FAIL: set_cookie({url}, {name}) rejected synchronously");
        return false;
    }
    match rx.recv_timeout(OP_TIMEOUT) {
        Ok(true) => true,
        Ok(false) => {
            println!("FAIL: CEF reported failure setting {name} for {url}");
            false
        }
        Err(_) => {
            println!("FAIL: timed out waiting for set_cookie({url}, {name})");
            false
        }
    }
}

fn read_cookies_blocking(url: &str) -> Vec<(String, String)> {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        println!("FAIL: no global cookie manager");
        return Vec::new();
    };
    let found = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let mut visitor = CollectCookies::new(found.clone(), Arc::new(Mutex::new(Some(tx))));
    let dispatched = manager.visit_url_cookies(Some(&CefString::from(url)), 1, Some(&mut visitor));
    if dispatched != 0 {
        // visit() "may never be called if no cookies are found" -- bounded
        // wait, not an indefinite one, resolves that case.
        let _ = rx.recv_timeout(OP_TIMEOUT);
    }
    let result = found.lock().unwrap().clone();
    result
}

fn main() {
    env_logger::init();

    if quietrabbit_lib::tier3_pane::dispatch_cef_subprocess() {
        return;
    }

    let root_cache_path =
        std::env::temp_dir().join(format!("qr_repro_224_{}", std::process::id()));
    println!("repro_224: root_cache_path = {}", root_cache_path.display());

    let _cef_init = quietrabbit_lib::tier3_pane::bootstrap::initialize_cef(&root_cache_path);

    let mut pane_window = PaneWindow::new();
    let proxy = pane_window.proxy();

    let claude_url = "https://claude.test/";
    let chatgpt_url = "https://chatgpt.test/";

    proxy
        .send_event(PaneCommand::Open {
            key: "claude".to_string(),
            url: claude_url.to_string(),
        })
        .expect("send Open(claude)");
    proxy
        .send_event(PaneCommand::Open {
            key: "chatgpt".to_string(),
            url: chatgpt_url.to_string(),
        })
        .expect("send Open(chatgpt)");

    // Let both panes construct their browsers (no per-pane RequestContext
    // to wait on any more -- see module doc). ~3s of pumps at ~60/s.
    for _ in 0..180 {
        pane_window.pump();
    }

    // --- Check 1: persistence across close/reopen ---------------------
    let set_ok = set_cookie_blocking(claude_url, "session", "claude-session-value");
    println!("set claude session cookie: {}", if set_ok { "ok" } else { "FAILED" });

    let before_close = read_cookies_blocking(claude_url);
    println!("claude cookies before close: {before_close:?}");

    proxy
        .send_event(PaneCommand::Close {
            key: "claude".to_string(),
        })
        .expect("send Close(claude)");
    for _ in 0..30 {
        pane_window.pump();
    }
    proxy
        .send_event(PaneCommand::Open {
            key: "claude".to_string(),
            url: claude_url.to_string(),
        })
        .expect("send reopen Open(claude)");
    for _ in 0..60 {
        pane_window.pump();
    }

    let after_reopen = read_cookies_blocking(claude_url);
    println!("claude cookies after close+reopen: {after_reopen:?}");
    let persisted = after_reopen
        .iter()
        .any(|(n, v)| n == "session" && v == "claude-session-value");
    println!(
        "CHECK 1 (persists across close/reopen): {}",
        if persisted { "PASS" } else { "FAIL" }
    );

    // --- Check 2: no cross-provider collision --------------------------
    set_cookie_blocking(chatgpt_url, "session", "chatgpt-session-value");

    let claude_cookies = read_cookies_blocking(claude_url);
    let chatgpt_cookies = read_cookies_blocking(chatgpt_url);
    println!("claude cookies: {claude_cookies:?}");
    println!("chatgpt cookies: {chatgpt_cookies:?}");

    let claude_has_own = claude_cookies
        .iter()
        .any(|(n, v)| n == "session" && v == "claude-session-value");
    let claude_leaked_chatgpt = claude_cookies.iter().any(|(_, v)| v == "chatgpt-session-value");
    let chatgpt_has_own = chatgpt_cookies
        .iter()
        .any(|(n, v)| n == "session" && v == "chatgpt-session-value");
    let chatgpt_leaked_claude = chatgpt_cookies.iter().any(|(_, v)| v == "claude-session-value");

    let no_collision =
        claude_has_own && !claude_leaked_chatgpt && chatgpt_has_own && !chatgpt_leaked_claude;
    println!(
        "CHECK 2 (no cross-provider collision, both panes open simultaneously): {}",
        if no_collision { "PASS" } else { "FAIL" }
    );

    println!(
        "repro_224: done. Check stderr above for any \
         'chrome_browser_context.cc' ERROR lines -- there should be NONE \
         (that class of failure is what this design replaces)."
    );

    std::process::exit(0);
}

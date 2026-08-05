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
//
// COOKIE PERSISTENCE (items.id=224 resolution, decisions.id=711): CEF's
// Chrome-runtime ChromeBrowserContext structurally rejects any second
// RequestContext (confirmed via gdb, src-tauri/examples/repro_224.rs) --
// every pane now shares CEF's one working global context/cookie jar
// instead (sync_window.rs no longer builds a per-pane context at all).
// This module is the actual lifecycle hook for per-provider persistence
// across app restarts: open_tier3_panes restores a provider's stored
// cookies into that shared jar (via CookieManager::set_cookie, awaited)
// *before* sending PaneCommand::Open, so they're already in place before
// the pane's first navigation; close_tier3_pane reads the jar back (via
// CookieManager::visit_url_cookies, awaited with a bounded timeout -- see
// that function's own doc on why a timeout is required, not optional) and
// persists via persistence::tier3_cookie_store *before* sending
// PaneCommand::Close. Both directions are best-effort: a cookie
// restore/persist failure is logged, never silently dropped, but must not
// block the pane open/close itself -- worse cookie fidelity is a real but
// recoverable degradation; a pane that won't open or won't close is not.
//
// CookieManager (and SetCookieCallback/CookieVisitor) are Send + Sync
// (cef::rc::RefGuard's own unconditional unsafe impl) and, per their own
// doc comment, "may be called on any thread" -- callable directly from
// this module's async Tokio context, no need to route through
// sync_window.rs's winit-thread-affine PaneManager at all.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cef::rc::Rc as _;
use cef::{
    wrap_cookie_visitor, wrap_set_cookie_callback, Basetime, CefString, Cookie, CookiePriority,
    CookieSameSite, CookieVisitor, ImplCookieManager, ImplCookieVisitor, ImplSetCookieCallback,
    SetCookieCallback, WrapCookieVisitor, WrapSetCookieCallback,
};
use tauri::State;
use tokio::sync::oneshot;
use winit::event_loop::EventLoopProxy;

use crate::auth::registry::KeyRegistry;
use crate::persistence::provider_store::{self, ProviderTier};
use crate::persistence::tier3_cookie_store::{self, StoredCookie};
use crate::tier3_pane::sync_window::PaneCommand;

/// How long to wait for a single CEF cookie-jar round trip
/// (set_cookie's completion callback, or visit_url_cookies' last-cookie
/// signal) before giving up. Generous relative to an in-process IPC call
/// (this is not a network round trip -- CEF's UI thread is on the same
/// machine), but bounded: visit_url_cookies' own doc confirms its visitor
/// "may never be called if no cookies are found" -- there is no
/// zero-results completion signal from the API at all, so a bounded wait
/// is the only way to resolve that case rather than hanging forever.
const COOKIE_OP_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Matches auth.rs/tier2.rs's own local copy exactly (not yet unified
/// behind a shared helper anywhere in this codebase -- see tier2.rs's own
/// header on Architecture Section 4.2 being a first-slice migration, not
/// yet a completed one).
fn key_hex(key: &[u8; crate::auth::kdf::MASTER_KEY_LEN]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// cef::Cookie <-> StoredCookie conversions
// ---------------------------------------------------------------------------
//
// same_site/priority: cef_cookie_same_site_t/cef_cookie_priority_t have no
// From<i32> in the vendored cef crate (confirmed) -- explicit match here,
// same defensive-conversion shape as provider_store.rs's
// ProviderTier::from_i64 (unrecognized value falls back to a documented
// safe default rather than panicking; a stored value should never be
// out of range, since it can only have come from get_raw() below, but a
// future schema/crate-version drift should degrade, not crash).

fn same_site_from_i32(v: i32) -> CookieSameSite {
    match v {
        1 => CookieSameSite::NO_RESTRICTION,
        2 => CookieSameSite::LAX_MODE,
        3 => CookieSameSite::STRICT_MODE,
        4 => CookieSameSite::NUM_VALUES,
        _ => CookieSameSite::UNSPECIFIED,
    }
}

fn priority_from_i32(v: i32) -> CookiePriority {
    match v {
        -1 => CookiePriority::LOW,
        1 => CookiePriority::HIGH,
        _ => CookiePriority::MEDIUM,
    }
}

fn stored_cookie_to_cef(c: &StoredCookie) -> Cookie {
    Cookie {
        name: CefString::from(c.name.as_str()),
        value: CefString::from(c.value.as_str()),
        domain: CefString::from(c.domain.as_str()),
        path: CefString::from(c.path.as_str()),
        secure: c.secure as i32,
        httponly: c.httponly as i32,
        creation: Basetime { val: c.creation },
        last_access: Basetime { val: c.last_access },
        has_expires: c.has_expires as i32,
        // has_expires=false -> expires is CEF's zeroed default, not a real
        // value (cef::Cookie's own has_expires-gates-expires contract, see
        // schema/tier3_cookies_001.sql's header) -- callers must check
        // has_expires, matching the round trip back in cef_cookie_to_stored.
        expires: Basetime {
            val: c.expires.unwrap_or(0),
        },
        same_site: same_site_from_i32(c.same_site),
        priority: priority_from_i32(c.priority),
        ..Default::default()
    }
}

fn cef_cookie_to_stored(c: &Cookie) -> StoredCookie {
    let has_expires = c.has_expires != 0;
    StoredCookie {
        name: c.name.to_string(),
        value: c.value.to_string(),
        domain: c.domain.to_string(),
        path: c.path.to_string(),
        secure: c.secure != 0,
        httponly: c.httponly != 0,
        same_site: c.same_site.get_raw() as i32,
        priority: c.priority.get_raw() as i32,
        has_expires,
        expires: if has_expires { Some(c.expires.val) } else { None },
        creation: c.creation.val,
        last_access: c.last_access.val,
    }
}

// ---------------------------------------------------------------------------
// CEF callback/visitor wrappers
// ---------------------------------------------------------------------------

// Signals a oneshot the moment CEF's UI thread reports set_cookie's
// completion. Arc<Mutex<Option<..>>>, not a bare oneshot::Sender: the
// wrap_set_cookie_callback! macro requires Clone on every field (the
// generated CookieManager-facing wrapper is cloned internally by CEF's
// own ref-counting) -- oneshot::Sender itself is not Clone, Arc is.
// take()'d exactly once, defensively, even though SetCookieCallback's own
// contract (unlike CookieVisitor's) guarantees exactly one on_complete
// call.
wrap_set_cookie_callback! {
    struct SetCookieDone {
        tx: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    }

    impl SetCookieCallback {
        fn on_complete(&self, success: std::os::raw::c_int) {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                let _ = tx.send(success != 0);
            }
        }
    }
}

// Accumulates every cookie CEF's UI thread delivers for a
// visit_url_cookies() call, signaling `done_tx` once (on the last cookie,
// count == total - 1) -- see COOKIE_OP_TIMEOUT's doc on why the caller
// still needs a bounded wait rather than relying on this signal alone
// (the zero-cookies case never calls `visit` at all).
//
// Buffers StoredCookie, not cef::Cookie: cef::Cookie's CefString fields
// wrap a raw, non-Send pointer (confirmed at compile time -- an earlier
// version of this buffer held `Cookie` directly and every #[tauri::command]
// using it failed to build, "future cannot be sent between threads
// safely... within `NonNull<_cef_string_utf16_t>`"). Converting inside
// `visit` itself -- a synchronous call on CEF's UI thread, nothing async
// about it -- means the buffer this async fn actually holds across its
// own `.await` is plain owned Strings/ints, which are Send.
wrap_cookie_visitor! {
    struct CollectCookiesVisitor {
        cookies: Arc<Mutex<Vec<StoredCookie>>>,
        done_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
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
                self.cookies.lock().unwrap().push(cef_cookie_to_stored(cookie));
            }
            if count + 1 >= total {
                if let Some(tx) = self.done_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
            1 // continue visiting; never request deletion
        }
    }
}

// ---------------------------------------------------------------------------
// Cookie restore/persist helpers
// ---------------------------------------------------------------------------

/// Loads provider_id's stored cookies (tier3_cookie_store) into CEF's one
/// global jar, scoped to `launch_url`. Best-effort per cookie: a rejected
/// or timed-out set_cookie is logged and skipped, never aborts the batch --
/// a partially-restored session (or none at all) is still a working pane,
/// just possibly logged out.
async fn restore_cookies_into_jar(user_id: &str, key_hex_str: &str, provider_id: &str, launch_url: &str) {
    let stored = match tier3_cookie_store::list_cookies(user_id, key_hex_str, provider_id).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "tier3_pane: could not read stored cookies for provider={provider_id}: {e} \
                 -- opening pane without cookie restore"
            );
            return;
        }
    };
    if stored.is_empty() {
        return;
    }

    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        log::warn!(
            "tier3_pane: CEF's global cookie manager unavailable -- cannot restore \
             cookies for provider={provider_id}"
        );
        return;
    };

    for cookie in &stored {
        let (tx, rx) = oneshot::channel();
        let mut callback = SetCookieDone::new(Arc::new(Mutex::new(Some(tx))));

        // Scoped block: CefString/Cookie wrap a non-Send raw pointer (see
        // CollectCookiesVisitor's doc) -- both must be constructed AND
        // dropped before the `.await` below, not held across it.
        let dispatched = {
            let url = CefString::from(launch_url);
            let cef_cookie = stored_cookie_to_cef(cookie);
            manager.set_cookie(Some(&url), Some(&cef_cookie), Some(&mut callback))
        };
        if dispatched == 0 {
            log::warn!(
                "tier3_pane: set_cookie rejected for provider={provider_id} name={} \
                 (invalid URL or cookies inaccessible)",
                cookie.name
            );
            continue;
        }

        match tokio::time::timeout(COOKIE_OP_TIMEOUT, rx).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => log::warn!(
                "tier3_pane: CEF reported failure restoring cookie provider={provider_id} name={}",
                cookie.name
            ),
            Ok(Err(_)) | Err(_) => log::warn!(
                "tier3_pane: timed out waiting for set_cookie completion, \
                 provider={provider_id} name={}",
                cookie.name
            ),
        }
    }
}

/// Reads back every cookie CEF's jar currently holds for `launch_url` and
/// persists them (full-replace) via tier3_cookie_store. Best-effort: a
/// failure here is logged, never propagated as a reason the pane can't
/// close.
async fn persist_cookies_from_jar(user_id: &str, key_hex_str: &str, provider_id: &str, launch_url: &str) {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        log::warn!(
            "tier3_pane: CEF's global cookie manager unavailable -- cannot persist \
             cookies for provider={provider_id}"
        );
        return;
    };

    let cookies: Arc<Mutex<Vec<StoredCookie>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = oneshot::channel();
    let mut visitor = CollectCookiesVisitor::new(cookies.clone(), Arc::new(Mutex::new(Some(tx))));

    // Scoped block: CefString wraps a non-Send raw pointer (see
    // CollectCookiesVisitor's doc) -- must not be held across the `.await`
    // below.
    let dispatched = {
        let url = CefString::from(launch_url);
        manager.visit_url_cookies(Some(&url), 1, Some(&mut visitor))
    };

    if dispatched != 0 {
        // Timeout, not just await: visit() "may never be called if no
        // cookies are found" (visit_url_cookies' own doc, confirmed
        // against the vendored cef crate) -- an empty jar for this
        // provider would otherwise hang here forever.
        let _ = tokio::time::timeout(COOKIE_OP_TIMEOUT, rx).await;
    }

    let collected: Vec<StoredCookie> = cookies.lock().unwrap().clone();

    if let Err(e) = tier3_cookie_store::upsert_cookies(user_id, key_hex_str, provider_id, &collected).await {
        log::warn!("tier3_pane: could not persist cookies for provider={provider_id}: {e}");
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
/// open. Cookie restore (items.id=224 resolution) is a separate, narrower
/// best-effort layer within each iteration -- see restore_cookies_into_jar's
/// own doc on why that failure mode does NOT abort the batch the same way.
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
    key_registry: State<'_, KeyRegistry>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // Not logged in => no per-provider cookie store to restore from at all
    // -- treated the same as "no stored cookies" (log + proceed), not a
    // reason to refuse opening the pane, matching restore_cookies_into_jar's
    // own best-effort framing.
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await;

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

        if let Some((user_id, key_hex_str)) = session.as_ref() {
            restore_cookies_into_jar(user_id, key_hex_str, &provider_id, &launch_url).await;
        } else {
            log::warn!(
                "tier3_pane: no resident session key -- opening provider={provider_id} \
                 without cookie restore"
            );
        }

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
/// why the explicit wake below is required, not optional. Cookie persist
/// (items.id=224 resolution) runs before the close is sent -- see
/// persist_cookies_from_jar's own doc; its failure is logged, never a
/// reason this command returns an error (the pane must still close).
#[tauri::command]
#[specta::specta]
pub async fn close_tier3_pane(
    provider_id: String,
    proxy: State<'_, EventLoopProxy<PaneCommand>>,
    key_registry: State<'_, KeyRegistry>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let session = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await;

    match (session, provider_store::get_provider(&provider_id).await) {
        (Some((user_id, key_hex_str)), Ok(Some(provider))) => {
            if let Some(launch_url) = provider.launch_url {
                persist_cookies_from_jar(&user_id, &key_hex_str, &provider_id, &launch_url).await;
            }
        }
        (None, _) => log::warn!(
            "tier3_pane: no resident session key -- closing provider={provider_id} \
             without cookie persist"
        ),
        (_, Ok(None)) => log::warn!(
            "tier3_pane: provider={provider_id} not found in catalog -- closing without \
             cookie persist"
        ),
        (_, Err(e)) => log::warn!(
            "tier3_pane: could not resolve provider={provider_id} for cookie persist: {e}"
        ),
    }

    proxy
        .send_event(PaneCommand::Close { key: provider_id })
        .map_err(|_| "tier3_pane event loop is no longer running".to_string())?;
    let _ = app_handle.run_on_main_thread(|| {});
    Ok(())
}

// src-tauri/src/commands/tier3_pane.rs
//
// Group 13 -- Tier 2/Tier 3 pane lifecycle & provider catalog.
// Commands: list_active_providers, open_tier3_panes, close_tier3_pane.
//
// items.id=202 piece 5 / items.id=223 connective tissue: neither item's own
// description enumerates an IPC command, but on-demand pane creation
// (items.id=223's whole point) needs something to actually call
// tier3_pane::pane_host's open/close from the frontend side -- this module
// is that something.
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
// open_tier3_panes/close_tier3_pane dispatch PaneCommand::Open/Close via
// AppHandle::run_on_main_thread (items.id=202 real positioning fix,
// 2026-08-07 -- replaces the old EventLoopProxy<PaneCommand>, dropped along
// with the rest of winit; see tier3_pane::pane_host's module docs).
// PaneHost lives in a main-thread-only thread-local (GTK objects aren't
// Send), reached via pane_host::dispatch() from inside the
// run_on_main_thread closure -- that closure itself only needs to be Send,
// which plain owned Strings/PaneKeys satisfy. launch_url is resolved
// server-side from provider_store, not accepted from the frontend -- the
// frontend only ever knows provider IDs.
//
// COOKIE PERSISTENCE (items.id=224 resolution, decisions.id=711): CEF's
// Chrome-runtime ChromeBrowserContext structurally rejects any second
// RequestContext (confirmed via gdb, src-tauri/examples/repro_224.rs) --
// every pane now shares CEF's one working global context/cookie jar
// instead (pane_host.rs no longer builds a per-pane context at all). This
// module is the actual lifecycle hook for per-provider persistence across
// app restarts: open_tier3_panes restores a provider's stored cookies into
// that shared jar (via CookieManager::set_cookie, awaited) *before*
// dispatching PaneCommand::Open, so they're already in place before the
// pane's first navigation; close_tier3_pane reads the jar back (via
// CookieManager::visit_url_cookies, awaited with a bounded timeout -- see
// that function's own doc on why a timeout is required, not optional) and
// persists via persistence::tier3_cookie_store *before* dispatching
// PaneCommand::Close. Both directions are best-effort: a cookie
// restore/persist failure is logged, never silently dropped, but must not
// block the pane open/close itself -- worse cookie fidelity is a real but
// recoverable degradation; a pane that won't open or won't close is not.
//
// CookieManager (and SetCookieCallback/CookieVisitor) are Send + Sync
// (cef::rc::RefGuard's own unconditional unsafe impl) and, per their own
// doc comment, "may be called on any thread" -- callable directly from
// this module's async Tokio context, no need to route through the
// main-thread-only pane host at all.

use std::collections::HashMap;
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

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::provider_store::{self, ProviderTier};
use crate::persistence::tier3_cookie_store::{self, StoredCookie};
use crate::tier3_pane::pane_host::{self, PaneCommand};
use crate::tier3_pane::PaneKey;

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

/// One pane's target region, as a fraction (0..1) of the main window's own
/// *content* area -- not absolute screen pixels. Dimensionless on purpose:
/// the frontend computes this from plain DOM geometry
/// (`getBoundingClientRect()` / `window.innerWidth`/`innerHeight`), with no
/// need for `devicePixelRatio` or a Tauri window-position API call. Under
/// single-window compositing (items.id=202 real positioning fix,
/// 2026-08-07, see pane_host.rs) this fraction is multiplied directly
/// against GTK's own live `GLArea` size inside `RenderState::render()` --
/// no Rust-side window-geometry query is involved at all, so a window
/// *move* alone stays correctly synced for free (GTK relayouts the GLArea
/// as an ordinary child widget), and even a resize only needs the GLArea's
/// own `resize` signal, not anything from `main.rs`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct PaneRectFraction {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `Vec`, not `HashMap`, to match this codebase's existing IPC-struct
/// convention -- no command signature anywhere else uses `HashMap`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct PaneLayoutEntry {
    pub provider_id: String,
    pub rect: PaneRectFraction,
}

/// Tauri-managed state holding the frontend's last-reported layout. Plain
/// data (`Arc<Mutex<HashMap<...>>>`), trivially `Send + Sync` -- unlike
/// `PaneHost`/`PaneManager` (GTK objects throughout), nothing here is
/// thread-affine, so this can be read directly from pane_host.rs's GLArea
/// `render`/`resize` closures via `AppHandle::state()`.
#[derive(Default)]
pub struct PaneLayoutState(pub Mutex<HashMap<PaneKey, PaneRectFraction>>);

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
        priority: c.priority.get_raw(),
        has_expires,
        expires: if has_expires {
            Some(c.expires.val)
        } else {
            None
        },
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
async fn restore_cookies_into_jar(
    user_id: &str,
    key_hex_str: &str,
    provider_id: &str,
    launch_url: &str,
) {
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
async fn persist_cookies_from_jar(
    user_id: &str,
    key_hex_str: &str,
    provider_id: &str,
    launch_url: &str,
) {
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

    if let Err(e) =
        tier3_cookie_store::upsert_cookies(user_id, key_hex_str, provider_id, &collected).await
    {
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
/// Dispatch is a single `AppHandle::run_on_main_thread` call per provider
/// (items.id=202 real positioning fix, 2026-08-07) -- unlike the old
/// `EventLoopProxy::send_event`, this both queues the work AND guarantees it
/// runs promptly: `run_on_main_thread` is Tauri/tao's own main-thread
/// dispatch queue, serviced as part of GTK's ordinary main-loop operation,
/// not a queue that needed a *separate* explicit wake call to be noticed
/// (see the old design's now-deleted note here about a real, empirically
/// confirmed dispatch-delay bug that required exactly that workaround).
#[tauri::command]
#[specta::specta]
pub async fn open_tier3_panes(
    provider_ids: Vec<String>,
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

        app_handle
            .run_on_main_thread(move || {
                pane_host::dispatch(PaneCommand::Open {
                    key: provider_id,
                    url: launch_url,
                })
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Closes one pane by provider ID. A no-op (not an error) if that provider
/// has no open pane -- `PaneManager::close_pane` already tolerates this
/// (pane_host.rs), and a caller racing a close against an already-closed
/// pane is a normal condition, not a failure. See open_tier3_panes' doc on
/// why a single `run_on_main_thread` call is dispatch and guaranteed-prompt
/// delivery in one step now. Cookie persist (items.id=224 resolution) runs
/// before the close is dispatched -- see persist_cookies_from_jar's own
/// doc; its failure is logged, never a reason this command returns an
/// error (the pane must still close).
#[tauri::command]
#[specta::specta]
pub async fn close_tier3_pane(
    provider_id: String,
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

    app_handle
        .run_on_main_thread(move || pane_host::dispatch(PaneCommand::Close { key: provider_id }))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// items.id=202 piece 4, real positioning fix 2026-08-07 -- stores the
/// frontend's live layout fractions (unchanged `PaneRectFraction` semantics:
/// fraction of the whole window's content area) and requests a redraw so the
/// GLArea's own `render` callback (pane_host.rs) picks up the new layout on
/// its very next tick. Called whenever the frontend's pane-dock region
/// resizes or `openPaneIds` changes (a different pane count changes the
/// column split even at the same dock size).
///
/// No window-geometry query here anymore. The old version queried
/// `window.inner_position()`/`inner_size()` to build an absolute
/// `PhysicalRect` for a separate OS window to sync against -- confirmed
/// broken on Wayland (pinned, wrong origin/size on this dev machine's
/// KDE/Wayland session) and the exact code this rewrite deletes rather than
/// patches. Under single-window compositing every pane's target is already
/// expressed relative to the GLArea's own size (render.rs's `render()`
/// reads `PaneLayoutState` directly), so there is nothing left to query.
#[tauri::command]
#[specta::specta]
pub async fn set_pane_layout(
    layout: Vec<PaneLayoutEntry>,
    layout_state: State<'_, PaneLayoutState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    {
        let mut map = layout_state.0.lock().unwrap();
        map.clear();
        for entry in layout {
            map.insert(entry.provider_id, entry.rect);
        }
    }

    app_handle
        .run_on_main_thread(pane_host::queue_draw)
        .map_err(|e| e.to_string())?;
    Ok(())
}

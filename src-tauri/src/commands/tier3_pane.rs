// src-tauri/src/commands/tier3_pane.rs
//
// Group 13 -- Tier 2/Tier 3 pane lifecycle & provider catalog.
// Commands: list_active_providers, open_tier3_panes, close_tier3_pane,
// set_pane_layout, forward_pane_mouse_click, forward_pane_mouse_move,
// forward_pane_mouse_wheel, forward_popup_mouse_click,
// forward_popup_mouse_move, forward_popup_mouse_wheel (items.id=234),
// forward_pane_key (items.id=332), adjust_pane_zoom (items.id=364),
// forward_popup_key (items.id=367).
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
use crate::persistence::provider_store;
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

/// Selector-screen-facing provider summary. `lane` matches the frontend's
/// `ProviderLane` string type (tier3AccessConfig.ts) verbatim -- no further
/// transformation needed on the TypeScript side.
///
/// items.id=427: providers has no tier column any more (Part 1's core
/// rule -- tier is a display label only, never stored). `lane` is now
/// derived here, at the display layer, from `provider_type` instead --
/// exactly the pattern the spec permits ("tier labels computed only at the
/// display layer"). Output is byte-identical to the old tier-based
/// derivation for the 4 known providers; a future provider_type this match
/// doesn't recognize falls back to the raw provider_type string, which
/// won't satisfy the frontend's closed `'tier2' | 'tier3'` type -- that's
/// Part 3c/5a's problem to solve when a new lane is actually needed, not
/// this one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Tier3ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub lane: String,
    pub login_required: bool,
    pub is_anonymous: bool,
    pub privacy_guardian_default_level: Option<provider_store::PrivacyGuardianDefaultLevel>,
}

fn lane_str(provider_type: &str) -> &str {
    match provider_type {
        "split_screen_web" => "tier2",
        "external_service" => "tier3",
        other => other,
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

/// The four modifier keys a browser `PointerEvent`/`WheelEvent` reports as
/// separate booleans (`shiftKey`/`ctrlKey`/`altKey`/`metaKey`) -- forwarded
/// as-is by `forward_pane_mouse_click`/`_move`/`_wheel` rather than
/// pre-converted to CEF's own bit-flag values client-side, so the one place
/// that knows CEF's actual flag constants stays in pane_host.rs
/// (`cef_modifiers_from_dom`).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct PaneEventModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Mirrors `cef::MouseButtonType`'s 3-button model -- not reused directly
/// since that type isn't `specta::Type`. The frontend maps a DOM
/// `PointerEvent.button` (0/1/2) to this before sending; button values with
/// no CEF equivalent (DOM also reports 3/4 for back/forward) are simply not
/// forwarded, same policy `cef_mouse_button_from_gdk` already applied to
/// GDK's own out-of-range button numbers.
/// Mirrors `cef::KeyEventType`'s 3 variants used here -- not reused directly
/// for the same reason `PaneMouseButton` isn't (that type isn't
/// `specta::Type`). No `Char`-vs-`RawKeyDown` ambiguity on the wire: the
/// frontend sends both explicitly for a printable keypress (see
/// `forward_pane_key`'s own doc), this enum just names which one a given
/// call is.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum PaneKeyEventType {
    RawKeyDown,
    Char,
    KeyUp,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum PaneMouseButton {
    Left,
    Middle,
    Right,
}

/// items.id=364: a pane's own Ctrl+=/Ctrl+-/Ctrl+0 shortcuts, intercepted by
/// the frontend (`PaneHitLayer.tsx`) before they'd otherwise reach CEF as
/// ordinary forwarded key events -- CEF's Chrome-runtime browser has no
/// window chrome of its own to interpret these as a zoom accelerator (that's
/// normally a browser-UI concern, not something Blink handles unprompted),
/// so the host app applies the zoom explicitly via
/// `BrowserHost::set_zoom_level` instead (see `adjust_pane_zoom`'s own doc).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, specta::Type)]
pub enum ZoomDirection {
    In,
    Out,
    Reset,
}

/// items.id=234: `tier3-popup-opened` event payload -- emitted the moment
/// `pane_host.rs`'s `drain_popup_requests` resolves and inserts a new
/// `PopupState` (immediately, not gated on the popup's first paint, so the
/// frontend can show the overlay promptly). Hand-declared here rather than
/// added to `collect_commands!` -- it's an event payload, not a command
/// argument, same convention as consent.rs's `ConsentRequestPayload`.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct PopupOpenedPayload {
    pub provider_id: String,
    pub rect: PaneRectFraction,
}

/// items.id=234: `tier3-popup-closed` event payload -- emitted for the two
/// close paths the frontend has no other way to learn about: the popup
/// self-closing (`window.close()` after a completed OAuth login) and the
/// parent pane navigating away. NOT emitted when the parent pane itself
/// closes (that popup teardown is synchronous inside `close_pane`, not
/// queued through the same per-tick drain) -- `Tier3AccessPane.tsx`
/// proactively clears its own popup state on pane-close instead, without
/// waiting for an event.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct PopupClosedPayload {
    pub provider_id: String,
}

// ---------------------------------------------------------------------------
// cef::Cookie <-> StoredCookie conversions
// ---------------------------------------------------------------------------
//
// same_site/priority: cef_cookie_same_site_t/cef_cookie_priority_t have no
// From<i32> in the vendored cef crate (confirmed) -- explicit match here,
// falling back to a documented safe default rather than panicking; a
// stored value should never be out of range, since it can only have come
// from get_raw() below, but a future schema/crate-version drift should
// degrade, not crash.

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
            lane: lane_str(&p.provider_type).to_string(),
            login_required: p.login_required,
            is_anonymous: p.is_anonymous,
            privacy_guardian_default_level: p.privacy_guardian_default_level,
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
/// delivery in one step now.
///
/// items.id=330: the close dispatch runs FIRST, cookie persist (items.id=224
/// resolution) after -- confirmed live this session (visible ~1s delay
/// before a closed pane actually stopped compositing on a newly-navigated-to
/// tab) that the previous persist-then-close ordering held the pane
/// on-screen for however long `persist_cookies_from_jar`'s cookie-jar round
/// trip took (up to `COOKIE_OP_TIMEOUT`, 500ms, per pane). Safe to reorder:
/// `persist_cookies_from_jar` reads CEF's *global* cookie manager by URL
/// (`cookie_manager_get_global_manager`), not anything tied to this specific
/// pane's `Browser` instance, so it works identically whether the browser
/// has already been torn down or not. Its failure is still only ever
/// logged, never a reason this command returns an error.
#[tauri::command]
#[specta::specta]
pub async fn close_tier3_pane(
    provider_id: String,
    key_registry: State<'_, KeyRegistry>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    {
        let provider_id = provider_id.clone();
        app_handle
            .run_on_main_thread(move || {
                pane_host::dispatch(PaneCommand::Close { key: provider_id })
            })
            .map_err(|e| e.to_string())?;
    }

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
            "tier3_pane: no resident session key -- closed provider={provider_id} \
             without cookie persist"
        ),
        (_, Ok(None)) => log::warn!(
            "tier3_pane: provider={provider_id} not found in catalog -- closed without \
             cookie persist"
        ),
        (_, Err(e)) => log::warn!(
            "tier3_pane: could not resolve provider={provider_id} for cookie persist: {e}"
        ),
    }

    Ok(())
}

/// items.id=359 pieces 4/5: makes `provider_id` (or none, to collapse to
/// the empty state) the one pane actually composited in the content
/// pane -- the rail+content-pane model's core mechanic. `None` no-ops if
/// nothing is active; a `provider_id` naming a pane that isn't currently
/// open (e.g. a race against a just-closed pane) is likewise a no-op at
/// the `pane_host::PaneManager::set_active_pane` layer (`panes.get_mut`
/// simply finds nothing), not an error -- same "a stale reference to a
/// pane is a normal race, not a failure" framing `close_tier3_pane`
/// already documents.
#[tauri::command]
#[specta::specta]
pub async fn set_active_pane(
    provider_id: Option<String>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::SetActivePane { key: provider_id })
        })
        .map_err(|e| e.to_string())
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

/// Forwards one mouse-button transition inside an open pane's on-screen
/// rect, hit-tested natively by the browser's own DOM (items.id=257 Path
/// B) -- the frontend's invisible per-pane hit-layer, one
/// absolutely-positioned `<div>` per open pane, kept in sync with the exact
/// same `PaneRectFraction` geometry `set_pane_layout` already reports (see
/// `paneLayout.ts`). Replaces the GDK input-shape click-routing mechanism
/// (items.id=257 Path A), which froze the whole client's Wayland pointer
/// input the moment it was given a non-empty region -- root-caused this
/// session (see pane_host.rs's module doc) to GDK's non-native child
/// windows never getting a real Wayland compositor surface to route input
/// to.
///
/// `x`/`y` arrive already pane-local, in the pane hit-div's own CSS pixel
/// space (`PointerEvent.offsetX`/`offsetY`) -- CSS pixels are the DOM's
/// device-independent unit, the same logical-pixel convention CEF's
/// `MouseEvent` expects, so no origin-subtraction or scale-factor division
/// is needed here the way the deleted GDK-path `cef_mouse_event` required:
/// the browser's own hit-testing already did the pane-scoping a GDK-side
/// `hit_test_pane` used to be needed for.
///
/// A no-op (not an error) if `provider_id` names a pane that has already
/// closed by the time this arrives -- an event racing a close is expected,
/// not a failure, matching `close_tier3_pane`'s own framing.
#[tauri::command]
#[specta::specta]
// IPC command signature mirrors the DOM event shape 1:1 (see doc above) --
// a wrapper struct would just move the sprawl, and would also touch the
// frontend's paneLayout.ts call site for no readability gain.
#[allow(clippy::too_many_arguments)]
pub async fn forward_pane_mouse_click(
    provider_id: String,
    x: f64,
    y: f64,
    button: PaneMouseButton,
    mouseup: bool,
    click_count: i32,
    buttons: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::MouseClick {
                key: provider_id,
                x,
                y,
                button,
                mouseup,
                click_count,
                buttons,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// Forwards a pointer move (or leave) inside an open pane's on-screen rect.
/// See `forward_pane_mouse_click`'s doc for the coordinate/no-op contract,
/// which this shares. The frontend coalesces these to at most one per
/// animation frame before sending -- the native GDK path this replaces ran
/// in-process at whatever rate the OS reported; this path crosses an IPC
/// boundary per call, so batching to the frame rate the compositor can
/// actually show avoids flooding it without a perceptible behavior change.
#[tauri::command]
#[specta::specta]
pub async fn forward_pane_mouse_move(
    provider_id: String,
    x: f64,
    y: f64,
    leaving: bool,
    buttons: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::MouseMove {
                key: provider_id,
                x,
                y,
                leaving,
                buttons,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// Forwards a wheel/scroll event inside an open pane's on-screen rect. See
/// `forward_pane_mouse_click`'s doc for the coordinate/no-op contract.
/// Unlike mouse-move, not throttled by the frontend -- CEF's own
/// scroll-momentum handling needs per-event delta fidelity, the same reason
/// the deleted GDK scroll handler forwarded every `scroll-event` unthrottled.
#[tauri::command]
#[specta::specta]
pub async fn forward_pane_mouse_wheel(
    provider_id: String,
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::MouseWheel {
                key: provider_id,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// Forwards one keyboard event into an open pane's CEF browser, once it has
/// DOM focus (`PaneHitLayer.tsx`'s per-pane hit-div already claims focus on
/// pointerdown, same element `forward_pane_mouse_click` fires from -- no
/// separate focus IPC call needed on the frontend side).
///
/// `windows_key_code` comes from the DOM `KeyboardEvent.keyCode` -- despite
/// being deprecated, it still follows the long-standing web-platform
/// convention of matching Windows virtual-key codes across engines, which is
/// exactly what CEF's `KeyEvent::windows_key_code` expects regardless of
/// platform. There is no real native/hardware keycode available here (input
/// arrives over IPC from a DOM event, not a native GTK/X11 event) --
/// `pane_host.rs`'s dispatch arm mirrors `windows_key_code` into CEF's
/// `native_key_code` as a documented best-effort stand-in rather than
/// leaving it zeroed or building a DOM-`code`-to-X11-keycode lookup table,
/// per items.id=332's scoping: enough to type into a login form, not
/// accelerator-perfect native-scancode fidelity.
///
/// `character` is 0 for non-printable keys (arrows, Enter, Backspace, ...)
/// and the key's single UTF-16 code unit otherwise -- the frontend sends a
/// `RawKeyDown` for every keydown, followed by a `Char` call only when
/// `character != 0`, mirroring CEF's own RAWKEYDOWN-then-CHAR convention for
/// text input.
///
/// Same no-op-on-already-closed-pane contract as `forward_pane_mouse_click`.
#[tauri::command]
#[specta::specta]
pub async fn forward_pane_key(
    provider_id: String,
    event_type: PaneKeyEventType,
    windows_key_code: i32,
    character: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::KeyEvent {
                key: provider_id,
                event_type,
                windows_key_code,
                character,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// items.id=364: steps an open pane's CEF zoom level up/down/reset (a
/// per-pane `f64` tracked in `PaneState`, see `pane_host.rs`) by forwarding
/// straight to `BrowserHost::set_zoom_level` -- the same mechanism a normal
/// browser's own Ctrl+=/Ctrl+-/Ctrl+0 accelerator would use, just driven from
/// this app's own frontend since CEF's Chrome-runtime browser has no browser
/// chrome of its own to bind that accelerator (see `ZoomDirection`'s own
/// doc). Does not reach a pane's popup, if it has one open -- popups get the
/// same `DEFAULT_ZOOM_LEVEL` applied once at creation (pane_host.rs's
/// `drain_popup_events`, items.id=366), but aren't wired to this command's
/// live in/out/reset stepping: they're short-lived OAuth login surfaces
/// (items.id=234), not something a user is expected to sit and adjust.
#[tauri::command]
#[specta::specta]
pub async fn adjust_pane_zoom(
    provider_id: String,
    direction: ZoomDirection,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::AdjustZoom {
                key: provider_id,
                direction,
            })
        })
        .map_err(|e| e.to_string())
}

/// items.id=367: popup counterpart to `forward_pane_key` -- same
/// windows_key_code/character/RawKeyDown-then-Char contract, see that
/// command's own doc. Missing entirely until now: `PopupHitLayer.tsx` only
/// ever forwarded mouse click/move/wheel (items.id=234's original scope),
/// so a popup could be clicked into and focused but never actually typed
/// into -- confirmed by Jason live, 2026-08-30 (Claude's Google sign-in
/// popup accepted focus/clicks but no keystrokes reached it, reproducing
/// even via a plain in-app pane switch away and back, not just after an
/// OS-level focus round-trip).
#[tauri::command]
#[specta::specta]
pub async fn forward_popup_key(
    provider_id: String,
    event_type: PaneKeyEventType,
    windows_key_code: i32,
    character: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::PopupKeyEvent {
                key: provider_id,
                event_type,
                windows_key_code,
                character,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// items.id=234: popup counterpart to `forward_pane_mouse_click` -- same
/// coordinate/no-op contract, except `x`/`y` are local to the popup's own
/// on-screen rect (`tier3-popup-opened`'s reported `rect`), not the parent
/// pane's. `provider_id` names the *parent* pane (popups have no separate
/// id-keyspace, see `PaneManager.popups`'s own doc, pane_host.rs).
#[tauri::command]
#[specta::specta]
// Same rationale as forward_pane_mouse_click's own allow: signature mirrors
// the DOM event shape 1:1, a wrapper struct would just move the sprawl.
#[allow(clippy::too_many_arguments)]
pub async fn forward_popup_mouse_click(
    provider_id: String,
    x: f64,
    y: f64,
    button: PaneMouseButton,
    mouseup: bool,
    click_count: i32,
    buttons: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::PopupMouseClick {
                key: provider_id,
                x,
                y,
                button,
                mouseup,
                click_count,
                buttons,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// items.id=234: popup counterpart to `forward_pane_mouse_move`.
#[tauri::command]
#[specta::specta]
pub async fn forward_popup_mouse_move(
    provider_id: String,
    x: f64,
    y: f64,
    leaving: bool,
    buttons: u16,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::PopupMouseMove {
                key: provider_id,
                x,
                y,
                leaving,
                buttons,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

/// items.id=234: popup counterpart to `forward_pane_mouse_wheel`.
#[tauri::command]
#[specta::specta]
pub async fn forward_popup_mouse_wheel(
    provider_id: String,
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
    modifiers: PaneEventModifiers,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    app_handle
        .run_on_main_thread(move || {
            pane_host::dispatch(PaneCommand::PopupMouseWheel {
                key: provider_id,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            })
        })
        .map_err(|e| e.to_string())
}

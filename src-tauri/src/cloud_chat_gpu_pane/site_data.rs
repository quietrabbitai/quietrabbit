//! Clearing a pane's site data (items.id=622).
//!
//! Deleting the cookie jar does not touch the rest of what a provider's page
//! stores in CEF's one global profile: localStorage, IndexedDB, Cache Storage
//! and service workers. A canary written to each survived a sign-out in the
//! items.id=622 step 0 run. The DevTools protocol can clear them per origin
//! (`Storage.clearDataForOrigin`, measured to clear all four on a page target;
//! sessionStorage goes with the browser), so this module
//!
//! - remembers which origins each pane has loaded (`note_origin`, fed by the
//!   pane's load handler, plus the provider's launch origin; kept until the
//!   key-leave cleanup, `forget_all`), and
//! - sends those clears, and `Network.clearBrowserCache`, through a pane's
//!   `BrowserHost` (`request_clear`), reporting when every answer has come back
//!   (`Tracker`), so the caller can close the panes after. It runs when an
//!   account's key leaves, not when a single pane closes: a provider's
//!   localStorage survives a pane being closed and reopened by the same account.
//!
//! Everything touching CEF runs on the GTK main thread, like the rest of
//! `pane_host`; the origin map is plain data behind a mutex.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{
    dictionary_value_create, wrap_dev_tools_message_observer, Browser, BrowserHost, CefString,
    DevToolsMessageObserver, ImplBrowserHost, ImplDevToolsMessageObserver, ImplDictionaryValue,
    Registration, WrapDevToolsMessageObserver,
};
use tokio::sync::oneshot;

use super::{lock_recover, PaneKey};

/// Origins one pane is remembered for. A page that opens more than this many
/// distinct origins is not a provider's chat page; the rest are not cleared
/// individually (the tmpfs profile still ends with the run).
const MAX_ORIGINS_PER_PANE: usize = 64;

static ORIGINS: Mutex<Option<HashMap<PaneKey, BTreeSet<String>>>> = Mutex::new(None);

/// First DevTools message id handed out; each call takes the next, process-wide,
/// so a result is never mistaken for another pane's.
static NEXT_MESSAGE_ID: AtomicI32 = AtomicI32::new(1000);

/// `scheme://host[:port]` of an http(s) URL, lower-cased; `None` for anything
/// else (about:blank, data:, blob:, file:, malformed).
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // drop userinfo
    let authority = authority.rsplit('@').next()?;
    if authority.is_empty()
        || !authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// Remember the origin of `url` as one `key`'s pane has loaded.
pub fn note_origin(key: &str, url: &str) {
    let Some(origin) = origin_of(url) else {
        return;
    };
    let mut map = lock_recover(&ORIGINS);
    let set = map
        .get_or_insert_with(HashMap::new)
        .entry(key.to_owned())
        .or_default();
    if set.len() < MAX_ORIGINS_PER_PANE {
        set.insert(origin);
    }
}

/// The origins remembered for `key`, in a stable order.
pub fn origins_for(key: &str) -> Vec<String> {
    lock_recover(&ORIGINS)
        .as_ref()
        .and_then(|m| m.get(key))
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Every origin any pane has loaded, deduplicated, in a stable order.
pub fn all_origins() -> Vec<String> {
    let map = lock_recover(&ORIGINS);
    let set: BTreeSet<&String> = map.iter().flat_map(|m| m.values()).flatten().collect();
    set.into_iter().cloned().collect()
}

/// Forget `key`'s origins. Not called when a pane closes: its site data stays
/// until a key leaves, and the cleanup then needs the origin.
#[cfg(test)]
pub fn forget(key: &str) {
    if let Some(map) = lock_recover(&ORIGINS).as_mut() {
        map.remove(key);
    }
}

/// Forget every pane's origins.
pub fn forget_all() {
    if let Some(map) = lock_recover(&ORIGINS).as_mut() {
        map.clear();
    }
}

/// Reports once every DevTools clear sent for it has been answered.
pub struct Tracker {
    remaining: AtomicUsize,
    done: Mutex<Option<oneshot::Sender<()>>>,
    /// Keeps each observer registered until the last answer.
    registrations: Mutex<Vec<Registration>>,
}

impl Tracker {
    /// A tracker that fires its receiver when it is told nothing is pending.
    /// Start it at 1 (a hold released by [`Tracker::release`]) so answers that
    /// arrive while messages are still being sent cannot finish it early.
    pub fn new() -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                remaining: AtomicUsize::new(1),
                done: Mutex::new(Some(tx)),
                registrations: Mutex::new(Vec::new()),
            }),
            rx,
        )
    }

    fn add(&self, n: usize) {
        self.remaining.fetch_add(n, Ordering::SeqCst);
    }

    fn one_done(&self) {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.finish();
        }
    }

    /// Drop the initial hold; fires if nothing is outstanding.
    pub fn release(&self) {
        self.one_done();
    }

    fn finish(&self) {
        if let Some(tx) = lock_recover(&self.done).take() {
            let _ = tx.send(());
        }
        lock_recover(&self.registrations).clear();
    }
}

wrap_dev_tools_message_observer! {
    struct ClearObserver {
        ids: Arc<Mutex<HashSet<i32>>>,
        tracker: Arc<Tracker>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: std::os::raw::c_int,
            success: std::os::raw::c_int,
            _result: Option<&[u8]>,
        ) {
            if lock_recover(&self.ids).remove(&message_id) {
                if success == 0 {
                    log::warn!(
                        "cloud_chat_gpu_pane::site_data: DevTools message {message_id} failed"
                    );
                }
                self.tracker.one_done();
            }
        }
    }
}

/// One DevTools call: a method and, for `Storage.clearDataForOrigin`, its origin.
struct Call {
    method: &'static str,
    origin: Option<String>,
}

fn calls_for(origins: &[String]) -> Vec<Call> {
    let mut calls = vec![Call {
        method: "Network.clearBrowserCache",
        origin: None,
    }];
    calls.extend(origins.iter().map(|o| Call {
        method: "Storage.clearDataForOrigin",
        origin: Some(o.clone()),
    }));
    calls
}

/// Ask `host`'s browser to clear its cache and, for each of `origins`, all of
/// that origin's site data (the profile is global, so any pane's browser can
/// reach every origin). `tracker` is told as each call is answered. Returns how
/// many calls were handed to CEF.
pub fn request_clear(host: &BrowserHost, origins: &[String], tracker: &Arc<Tracker>) -> usize {
    let calls = calls_for(origins);
    let ids: Vec<i32> = calls
        .iter()
        .map(|_| NEXT_MESSAGE_ID.fetch_add(1, Ordering::SeqCst))
        .collect();

    // Counted and registered before anything is sent: answers arrive on CEF's
    // UI thread while this thread is still sending.
    tracker.add(ids.len());
    let pending = Arc::new(Mutex::new(ids.iter().copied().collect::<HashSet<i32>>()));
    let mut observer = ClearObserver::new(pending, tracker.clone());
    if let Some(registration) = host.add_dev_tools_message_observer(Some(&mut observer)) {
        lock_recover(&tracker.registrations).push(registration);
    }

    let mut sent = 0;
    for (call, id) in calls.iter().zip(&ids) {
        let params = call.origin.as_ref().and_then(|origin| {
            let params = dictionary_value_create()?;
            params.set_string(
                Some(&CefString::from("origin")),
                Some(&CefString::from(origin.as_str())),
            );
            params.set_string(
                Some(&CefString::from("storageTypes")),
                Some(&CefString::from("all")),
            );
            Some(params)
        });
        let mut params = params;
        let accepted = host.execute_dev_tools_method(
            *id,
            Some(&CefString::from(call.method)),
            params.as_mut(),
        );
        if accepted != 0 {
            sent += 1;
        } else {
            log::warn!(
                "cloud_chat_gpu_pane::site_data: CEF refused {} ({:?})",
                call.method,
                call.origin
            );
            // Never sent, so no answer will come: settle it here.
            tracker.one_done();
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_scheme_host_and_port_only() {
        assert_eq!(
            origin_of("https://duck.ai/"),
            Some("https://duck.ai".into())
        );
        assert_eq!(
            origin_of("https://Chat.OpenAI.com/c/abc?x=1#y"),
            Some("https://chat.openai.com".into())
        );
        assert_eq!(
            origin_of("http://localhost:8080/a"),
            Some("http://localhost:8080".into())
        );
        assert_eq!(
            origin_of("https://user:pw@claude.ai/chats"),
            Some("https://claude.ai".into())
        );
        assert_eq!(
            origin_of("https://[::1]:443/"),
            Some("https://[::1]:443".into())
        );
    }

    #[test]
    fn non_web_and_malformed_urls_have_no_origin() {
        for url in [
            "about:blank",
            "data:text/html,hi",
            "blob:https://a.com/uuid",
            "file:///etc/passwd",
            "chrome://version",
            "https://",
            "https:///path",
            "https://bad host/",
            "https://a.com\\evil",
            "",
            "duck.ai",
        ] {
            assert_eq!(origin_of(url), None, "{url}");
        }
    }

    #[test]
    fn origins_are_kept_per_pane_deduplicated_and_forgotten() {
        let (a, b) = ("site-data-test-a", "site-data-test-b");
        forget(a);
        forget(b);
        note_origin(a, "https://duck.ai/x");
        note_origin(a, "https://duck.ai/y");
        note_origin(a, "https://improving.duckduckgo.com/e");
        note_origin(a, "about:blank");
        note_origin(b, "https://claude.ai/");
        assert_eq!(
            origins_for(a),
            vec![
                "https://duck.ai".to_owned(),
                "https://improving.duckduckgo.com".to_owned()
            ]
        );
        assert_eq!(origins_for(b), vec!["https://claude.ai".to_owned()]);
        forget(a);
        assert!(origins_for(a).is_empty());
        assert_eq!(
            origins_for(b).len(),
            1,
            "forgetting one pane leaves the other's"
        );
        forget(b);
    }

    #[test]
    fn all_origins_is_the_union_across_panes_and_survives_a_pane_going_away() {
        let (a, b) = ("site-data-test-union-a", "site-data-test-union-b");
        note_origin(a, "https://union-a.example/x");
        note_origin(b, "https://union-b.example/y");
        note_origin(b, "https://union-a.example/z");
        let all = all_origins();
        assert_eq!(
            all.iter()
                .filter(|o| *o == "https://union-a.example")
                .count(),
            1
        );
        assert!(all.contains(&"https://union-b.example".to_owned()));
        let sorted = {
            let mut v = all.clone();
            v.sort();
            v
        };
        assert_eq!(all, sorted, "stable order");
        // closing a pane does not forget its origins: the key-leave cleanup needs them
        assert!(all_origins().contains(&"https://union-a.example".to_owned()));
        forget(a);
        forget(b);
    }

    #[test]
    fn a_pane_stops_remembering_after_the_cap() {
        let key = "site-data-test-cap";
        forget(key);
        for i in 0..(MAX_ORIGINS_PER_PANE + 10) {
            note_origin(key, &format!("https://h{i}.example/"));
        }
        assert_eq!(origins_for(key).len(), MAX_ORIGINS_PER_PANE);
        forget(key);
    }

    #[test]
    fn the_call_list_is_one_cache_clear_then_one_storage_clear_per_origin() {
        let calls = calls_for(&["https://a.com".to_owned(), "https://b.com".to_owned()]);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].method, "Network.clearBrowserCache");
        assert!(calls[0].origin.is_none());
        assert_eq!(calls[1].method, "Storage.clearDataForOrigin");
        assert_eq!(calls[2].origin.as_deref(), Some("https://b.com"));
        assert_eq!(calls_for(&[]).len(), 1);
    }

    #[tokio::test]
    async fn a_tracker_fires_only_after_every_answer_and_the_release() {
        let (tracker, mut rx) = Tracker::new();
        tracker.add(2);
        tracker.one_done();
        tracker.release();
        assert!(rx.try_recv().is_err(), "one answer is still outstanding");
        tracker.one_done();
        assert!(rx.await.is_ok());
    }

    #[tokio::test]
    async fn a_tracker_with_nothing_sent_fires_on_release() {
        let (tracker, rx) = Tracker::new();
        tracker.release();
        assert!(rx.await.is_ok());
    }
}

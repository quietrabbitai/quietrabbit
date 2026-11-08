// src-tauri/src/cloud_chat_gpu_pane/nav_confirm.rs
//
// items.id=873, decisions.id=934: the pending "open this link in your browser?"
// requests a pane's navigation policy raises, and the notices for blocked
// schemes. Pure bookkeeping, no CEF types and no GTK.
//
// FLOW. A CEF handler (UI thread) cancels the load and sends a `NavBlocked` over
// a channel; the GTK tick drains it (pane_host.rs) into `register_confirm` here
// and emits a Tauri event; the frontend modal answers through the IPC command
// `answer_cloud_chat_nav_confirm`, which carries only the request id and the
// decision. THE URL STAYS HERE: the frontend never supplies what gets opened
// (provider_credentials/opener.rs precedent), and only an http(s) URL without
// userinfo ever reaches the opener, after a confirm.
//
// COALESCING (a page that keeps navigating cannot flood the user):
//   - one pending confirm per pane;
//   - a repeat of the same host for that pane while it is pending is dropped;
//   - a different host replaces the older request (the older one is reported as
//     cancelled so its modal dismisses);
//   - at most MAX_PENDING requests overall (the oldest is cancelled);
//   - identical notices for the same pane and scheme within NOTICE_WINDOW collapse.
// EXPIRY. A pane's requests are purged when it closes (`purge_pane`). An unknown
// or already-used id does nothing (`take` returns None, the caller logs).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cloud_chat_gpu_pane::{lock_recover, PaneKey};

/// Pending confirms held at once.
pub const MAX_PENDING: usize = 8;

/// Identical notices for one pane and scheme inside this window show once.
pub const NOTICE_WINDOW: Duration = Duration::from_secs(2);

/// What a handler asks the host to show. Sent from CEF's UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavBlocked {
    pub pane_key: PaneKey,
    pub kind: NavBlockKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavBlockKind {
    /// An http(s) target off the declared hosts: confirm, naming `host`.
    Confirm { host: String, url: String },
    /// A blocked scheme (`Some`) or a malformed link (`None`): notice only.
    Notice { scheme: Option<String> },
}

/// Event `cloud-chat-nav-confirm`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavConfirmPayload {
    pub request_id: String,
    pub provider_id: String,
    pub host: String,
}

/// Event `cloud-chat-nav-notice`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavNoticePayload {
    pub provider_id: String,
    pub scheme: Option<String>,
}

/// Event `cloud-chat-nav-confirm-cancelled`: the pane's pending confirm, if
/// the frontend shows one, is gone.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NavConfirmCancelledPayload {
    pub provider_id: String,
}

/// A request that has been registered and is waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConfirm {
    pub pane_key: PaneKey,
    pub host: String,
    pub url: String,
}

/// What registering a confirm did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Registered {
    /// The new request id, or None when it was dropped as a repeat.
    pub request_id: Option<String>,
    /// Panes whose earlier pending confirm was replaced or evicted.
    pub cancelled_panes: Vec<PaneKey>,
}

struct Entry {
    id: String,
    seq: u64,
    pending: PendingConfirm,
}

#[derive(Default)]
struct Inner {
    /// At most one entry per pane.
    by_pane: HashMap<PaneKey, Entry>,
    last_notice: HashMap<(PaneKey, Option<String>), Instant>,
}

/// The pending-request store. One process-wide instance (`store()`).
pub struct NavConfirmStore {
    inner: Mutex<Inner>,
    next: AtomicU64,
}

impl Default for NavConfirmStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NavConfirmStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            next: AtomicU64::new(1),
        }
    }

    /// Registers a confirm for `pane_key`. See the module doc for coalescing.
    pub fn register_confirm(&self, pane_key: &str, host: String, url: String) -> Registered {
        let mut g = lock_recover(&self.inner);
        let mut cancelled = Vec::new();

        if let Some(existing) = g.by_pane.get(pane_key) {
            if existing.pending.host == host {
                return Registered::default();
            }
            cancelled.push(pane_key.to_owned());
            g.by_pane.remove(pane_key);
        }

        if g.by_pane.len() >= MAX_PENDING {
            if let Some(oldest) = g
                .by_pane
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(k, _)| k.clone())
            {
                g.by_pane.remove(&oldest);
                cancelled.push(oldest);
            }
        }

        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        let id = format!("nav-{seq}");
        g.by_pane.insert(
            pane_key.to_owned(),
            Entry {
                id: id.clone(),
                seq,
                pending: PendingConfirm {
                    pane_key: pane_key.to_owned(),
                    host,
                    url,
                },
            },
        );
        Registered {
            request_id: Some(id),
            cancelled_panes: cancelled,
        }
    }

    /// Removes and returns the request `id`. None for an unknown, used or
    /// purged id.
    pub fn take(&self, id: &str) -> Option<PendingConfirm> {
        let mut g = lock_recover(&self.inner);
        let key = g
            .by_pane
            .iter()
            .find(|(_, e)| e.id == id)
            .map(|(k, _)| k.clone())?;
        g.by_pane.remove(&key).map(|e| e.pending)
    }

    /// Drops everything held for `pane_key` (the pane closed). Returns whether a
    /// confirm was pending, so the caller can tell the frontend to dismiss it.
    pub fn purge_pane(&self, pane_key: &str) -> bool {
        let mut g = lock_recover(&self.inner);
        g.last_notice.retain(|(k, _), _| k != pane_key);
        g.by_pane.remove(pane_key).is_some()
    }

    /// Whether a notice for (`pane_key`, `scheme`) should show now. `now` is a
    /// parameter so tests do not sleep.
    pub fn allow_notice(&self, pane_key: &str, scheme: Option<&str>, now: Instant) -> bool {
        let mut g = lock_recover(&self.inner);
        g.last_notice
            .retain(|_, at| now.saturating_duration_since(*at) < NOTICE_WINDOW);
        let key = (pane_key.to_owned(), scheme.map(str::to_owned));
        if g.last_notice.contains_key(&key) {
            return false;
        }
        g.last_notice.insert(key, now);
        true
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        lock_recover(&self.inner).by_pane.len()
    }
}

/// The process-wide store.
pub fn store() -> &'static NavConfirmStore {
    static STORE: OnceLock<NavConfirmStore> = OnceLock::new();
    STORE.get_or_init(NavConfirmStore::new)
}

/// The URL an Open answer may hand to the system browser, or None.
///
/// Re-checked at answer time even though classify() produced it: http(s) only,
/// no userinfo, bounded length. Anything else is never passed on.
pub fn openable_url(url: &str) -> Option<String> {
    if url.len() > crate::cloud_chat_gpu_pane::nav_policy::MAX_URL_BYTES {
        return None;
    }
    let u = reqwest::Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.host_str().is_none()
    {
        return None;
    }
    Some(u.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(s: &NavConfirmStore, pane: &str, host: &str) -> Registered {
        s.register_confirm(pane, host.to_owned(), format!("https://{host}/"))
    }

    #[test]
    fn a_registered_request_is_taken_once_and_an_unknown_or_used_id_does_nothing() {
        let s = NavConfirmStore::new();
        let id = reg(&s, "p1", "a.example.test").request_id.unwrap();
        let taken = s.take(&id).expect("pending");
        assert_eq!(taken.pane_key, "p1");
        assert_eq!(taken.host, "a.example.test");
        assert_eq!(taken.url, "https://a.example.test/");
        assert!(s.take(&id).is_none(), "used id");
        assert!(s.take("nav-999").is_none(), "unknown id");
        assert!(s.take("").is_none());
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn a_repeat_of_the_same_host_for_a_pane_is_dropped() {
        let s = NavConfirmStore::new();
        let first = reg(&s, "p1", "a.example.test");
        assert!(first.request_id.is_some());
        let again = reg(&s, "p1", "a.example.test");
        assert_eq!(again, Registered::default());
        assert_eq!(s.pending_count(), 1);
        // The original request is still the live one.
        assert!(s.take(&first.request_id.unwrap()).is_some());
    }

    #[test]
    fn a_different_host_replaces_the_older_request_and_reports_it_cancelled() {
        let s = NavConfirmStore::new();
        let old = reg(&s, "p1", "a.example.test").request_id.unwrap();
        let new = reg(&s, "p1", "b.example.test");
        assert_eq!(new.cancelled_panes, vec!["p1".to_owned()]);
        assert!(s.take(&old).is_none(), "the replaced id is gone");
        let taken = s.take(&new.request_id.unwrap()).unwrap();
        assert_eq!(taken.host, "b.example.test");
    }

    #[test]
    fn panes_are_independent() {
        let s = NavConfirmStore::new();
        let a = reg(&s, "p1", "a.example.test").request_id.unwrap();
        let b = reg(&s, "p2", "a.example.test").request_id.unwrap();
        assert_ne!(a, b);
        assert_eq!(s.pending_count(), 2);
        assert!(s.take(&a).is_some());
        assert!(s.take(&b).is_some());
    }

    #[test]
    fn the_cap_evicts_the_oldest_and_reports_it() {
        let s = NavConfirmStore::new();
        let mut ids = Vec::new();
        for i in 0..MAX_PENDING {
            ids.push(
                reg(&s, &format!("p{i}"), "a.example.test")
                    .request_id
                    .unwrap(),
            );
        }
        assert_eq!(s.pending_count(), MAX_PENDING);
        let over = reg(&s, "extra", "a.example.test");
        assert_eq!(over.cancelled_panes, vec!["p0".to_owned()]);
        assert_eq!(s.pending_count(), MAX_PENDING);
        assert!(s.take(&ids[0]).is_none(), "the oldest was evicted");
        assert!(s.take(&ids[1]).is_some());
        assert!(s.take(&over.request_id.unwrap()).is_some());
    }

    #[test]
    fn closing_a_pane_purges_its_request() {
        let s = NavConfirmStore::new();
        let id = reg(&s, "p1", "a.example.test").request_id.unwrap();
        let other = reg(&s, "p2", "a.example.test").request_id.unwrap();
        assert!(s.purge_pane("p1"));
        assert!(!s.purge_pane("p1"), "nothing left to purge");
        assert!(s.take(&id).is_none(), "a purged id does nothing");
        assert!(s.take(&other).is_some(), "other panes keep theirs");
    }

    #[test]
    fn identical_notices_collapse_inside_the_window_only() {
        let s = NavConfirmStore::new();
        let t0 = Instant::now();
        assert!(s.allow_notice("p1", Some("data"), t0));
        assert!(!s.allow_notice("p1", Some("data"), t0 + Duration::from_millis(500)));
        // A different scheme, a malformed link and another pane each show.
        assert!(s.allow_notice("p1", Some("blob"), t0 + Duration::from_millis(500)));
        assert!(s.allow_notice("p1", None, t0 + Duration::from_millis(500)));
        assert!(s.allow_notice("p2", Some("data"), t0 + Duration::from_millis(500)));
        // After the window the same notice shows again.
        assert!(s.allow_notice(
            "p1",
            Some("data"),
            t0 + NOTICE_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn only_http_and_https_without_userinfo_are_openable() {
        assert_eq!(
            openable_url("https://a.example.test/p?q=1").as_deref(),
            Some("https://a.example.test/p?q=1")
        );
        assert_eq!(
            openable_url("http://a.example.test/").as_deref(),
            Some("http://a.example.test/")
        );
        for url in [
            "",
            "file:///etc/passwd",
            "data:text/html,x",
            "javascript:alert(1)",
            "blob:https://a.example.test/x",
            "mailto:a@example.test",
            "chrome://settings",
            "https://user@a.example.test/",
            "https://user:pw@a.example.test/",
            "not a url",
        ] {
            assert_eq!(openable_url(url), None, "{url:?}");
        }
        let long = format!("https://a.example.test/{}", "a".repeat(3000));
        assert_eq!(openable_url(&long), None);
    }
}

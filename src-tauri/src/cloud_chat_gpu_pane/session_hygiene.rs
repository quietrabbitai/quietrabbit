//! Cloud Chat session hygiene (items.id=622): what happens to CEF's one global
//! profile when an account's key leaves memory.
//!
//! CEF's Chrome runtime (151, and 156 per items.id=872) has a single global profile (decisions.id=711), so
//! the cookies, localStorage and open pages of one signed-in account are, to
//! the browser, simply the profile. Without this module a sign-out left the
//! panes, their cookies and their storage alive, and the next sign-in
//! re-attached the same pages.
//!
//! **The trigger** is [`KeyRegistry::on_key_leave`]: it fires when a key is
//! cleared (every lock path, logout, a failed sign-in rolling back) or replaced
//! by another account's. It is the boundary the item is about: a screen lock
//! that keeps the key (items.id=576) never fires it. The observer registered
//! here runs on the clearing thread, only bumps counters and queues the event,
//! so key clearing cannot wait on or fail because of CEF (items.id=621).
//!
//! **The cleanup**, one queued event at a time, under the jar gate:
//! 1. clear the site data of every origin any pane loaded through one open
//!    pane's DevTools (`site_data`) and wait for the answers, bounded;
//! 2. close every pane;
//! 3. delete all cookies from the global jar and flush the store;
//! 4. once more after teardown settles (a page can set a cookie while closing).
//!
//! **The jar gate** orders everything else that touches the jar. The cookie
//! restore (`open_cloud_chat_panes`), the pane-close persist and the periodic
//! snapshot all take the gate first. It refuses while a cleanup is pending, and
//! hands out the jar epoch so a holder can tell, before writing, that a key
//! left since it read the key. A slow cleanup therefore can never wipe the next
//! account's freshly restored cookies, and a late snapshot can never write the
//! next account's jar into the previous account's store.
//!
//! **The stored set stays fresh** by a snapshot of every open provider's
//! cookies every [`SNAPSHOT_INTERVAL`] (plus the persist when a pane closes).
//! Nothing waits for CEF at lock time, so a lock loses at most one interval of
//! cookie changes and can leak nothing.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cef::rc::Rc as _;
use cef::{
    wrap_completion_callback, wrap_delete_cookies_callback, CompletionCallback,
    DeleteCookiesCallback, ImplCompletionCallback, ImplCookieManager, ImplDeleteCookiesCallback,
    WrapCompletionCallback, WrapDeleteCookiesCallback,
};
use tokio::sync::{mpsc, oneshot, Notify};

use super::{lock_recover, site_data};
use crate::auth::registry::{KeyLeft, KeyRegistry};

/// How long a jar-gate user waits for a pending cleanup (and the previous
/// holder) before giving up. Failing closed: the caller skips its restore or
/// persist rather than touching a jar that is about to be wiped.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Time between snapshots of the open providers' cookies into the encrypted store.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(30);

/// How long the cleanup waits for the DevTools answers before closing panes anyway.
const SITE_DATA_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a cookie delete or store flush may take.
const COOKIE_OP_TIMEOUT: Duration = Duration::from_secs(2);

/// Pause before the second cookie delete, for a closing page's last writes.
const TEARDOWN_SETTLE: Duration = Duration::from_millis(300);

/// Orders jar users against key-leave cleanups.
pub struct JarGate {
    /// Bumped when a key leaves; a [`JarGuard`] remembers the value it started with.
    epoch: AtomicU64,
    /// Cleanups queued or running.
    pending: AtomicUsize,
    idle: Notify,
    gate: tokio::sync::Mutex<()>,
}

impl JarGate {
    pub fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            pending: AtomicUsize::new(0),
            idle: Notify::new(),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// A key left: from now no jar user may proceed until the cleanup is done,
    /// and any guard held across this point is stale. Synchronous and cheap.
    pub fn note_leave(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    /// One cleanup finished (or was abandoned).
    pub fn finish_cleanup(&self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    async fn wait_idle(&self, deadline: tokio::time::Instant) -> bool {
        loop {
            // Registered before the check so a finish in between is not missed.
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.pending() == 0;
            }
        }
    }

    /// Wait (up to `limit`) until no cleanup is pending and the gate is free,
    /// then hold it. `None` on timeout.
    pub async fn acquire_within(&self, limit: Duration) -> Option<JarGuard<'_>> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if !self.wait_idle(deadline).await {
                return None;
            }
            let guard = tokio::time::timeout_at(deadline, self.gate.lock())
                .await
                .ok()?;
            // A key may have left while the gate was being waited for.
            if self.pending() == 0 {
                return Some(JarGuard {
                    _guard: guard,
                    gate: self,
                    epoch: self.epoch.load(Ordering::SeqCst),
                });
            }
        }
    }
}

impl Default for JarGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Holds the jar gate. Dropping it releases it.
pub struct JarGuard<'a> {
    _guard: tokio::sync::MutexGuard<'a, ()>,
    gate: &'a JarGate,
    epoch: u64,
}

impl JarGuard<'_> {
    /// Whether no key has left since this guard was taken. A holder that read
    /// the key after taking the guard checks this right before it writes.
    pub fn still_current(&self) -> bool {
        self.gate.epoch.load(Ordering::SeqCst) == self.epoch
    }
}

struct State {
    gate: JarGate,
    /// Provider ids whose panes the commands opened and have not closed.
    open: Mutex<BTreeSet<String>>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        gate: JarGate::new(),
        open: Mutex::new(BTreeSet::new()),
    })
}

/// Take the process-wide jar gate (see [`JarGate::acquire_within`]).
pub async fn acquire_jar() -> Option<JarGuard<'static>> {
    state().gate.acquire_within(ACQUIRE_TIMEOUT).await
}

pub fn note_open(provider_id: &str) {
    lock_recover(&state().open).insert(provider_id.to_owned());
}

pub fn note_closed(provider_id: &str) {
    lock_recover(&state().open).remove(provider_id);
}

pub fn open_ids() -> Vec<String> {
    lock_recover(&state().open).iter().cloned().collect()
}

fn clear_open() {
    lock_recover(&state().open).clear();
}

wrap_delete_cookies_callback! {
    struct DeleteDone {
        tx: Arc<Mutex<Option<oneshot::Sender<i32>>>>,
    }

    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: std::os::raw::c_int) {
            if let Some(tx) = lock_recover(&self.tx).take() {
                let _ = tx.send(num_deleted);
            }
        }
    }
}

wrap_completion_callback! {
    struct FlushDone {
        tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            if let Some(tx) = lock_recover(&self.tx).take() {
                let _ = tx.send(());
            }
        }
    }
}

/// Delete every cookie of every host from CEF's global jar. `Some(n)` with the
/// count CEF reports, `None` if CEF refused or did not answer in time.
async fn delete_all_cookies() -> Option<i32> {
    let manager = cef::cookie_manager_get_global_manager(None)?;
    let (tx, rx) = oneshot::channel();
    let mut callback = DeleteDone::new(Arc::new(Mutex::new(Some(tx))));
    if manager.delete_cookies(None, None, Some(&mut callback)) == 0 {
        return None;
    }
    tokio::time::timeout(COOKIE_OP_TIMEOUT, rx).await.ok()?.ok()
}

/// Flush the global cookie store to its backing file.
async fn flush_cookie_store() {
    let Some(manager) = cef::cookie_manager_get_global_manager(None) else {
        return;
    };
    let (tx, rx) = oneshot::channel();
    let mut callback = FlushDone::new(Arc::new(Mutex::new(Some(tx))));
    if manager.flush_store(Some(&mut callback)) != 0 {
        let _ = tokio::time::timeout(COOKIE_OP_TIMEOUT, rx).await;
    }
}

/// Everything a key leaving means for CEF. Runs under the jar gate.
async fn run_cleanup(app: &tauri::AppHandle, left: &KeyLeft) {
    log::info!(
        "session_hygiene: key left (user={}, {:?}), clearing the Cloud Chat session",
        left.user_id,
        left.reason
    );

    // 1. Site data, while the browsers still exist. The tracker lives on the
    // main thread; only its receiver comes back here.
    let (sent_tx, sent_rx) = oneshot::channel::<oneshot::Receiver<()>>();
    let queued = app.run_on_main_thread(move || {
        let (tracker, done) = site_data::Tracker::new();
        super::pane_host::clear_site_data_all(&tracker);
        tracker.release();
        let _ = sent_tx.send(done);
    });
    if queued.is_ok() {
        let started = tokio::time::Instant::now();
        match tokio::time::timeout(SITE_DATA_TIMEOUT, async {
            match sent_rx.await {
                Ok(done) => done.await.is_ok(),
                Err(_) => false,
            }
        })
        .await
        {
            Ok(true) => log::info!(
                "session_hygiene: site data cleared in {:?}",
                started.elapsed()
            ),
            Ok(false) | Err(_) => {
                log::warn!(
                    "session_hygiene: site data clear was not confirmed, closing panes anyway"
                )
            }
        }
    } else {
        log::warn!("session_hygiene: could not reach the main thread to clear site data");
    }

    // 2. Close every pane.
    if let Err(e) = app.run_on_main_thread(super::pane_host::close_all_panes) {
        log::warn!("session_hygiene: could not reach the main thread to close panes: {e}");
    }
    clear_open();
    site_data::forget_all();

    // 3. and 4. The jar, twice.
    let first = delete_all_cookies().await;
    flush_cookie_store().await;
    tokio::time::sleep(TEARDOWN_SETTLE).await;
    let second = delete_all_cookies().await;
    flush_cookie_store().await;
    log::info!("session_hygiene: cookies deleted (first pass {first:?}, second pass {second:?})");
}

/// Hook the cleanup and the snapshot task up. Call once at startup, only when
/// CEF initialised.
pub fn install(app: &tauri::AppHandle, key_registry: &KeyRegistry) {
    let (tx, rx) = mpsc::unbounded_channel::<KeyLeft>();
    key_registry.on_key_leave(move |left| {
        state().gate.note_leave();
        if tx.send(left.clone()).is_err() {
            // No consumer left: nothing will clean up, so do not leave the gate shut.
            state().gate.finish_cleanup();
        }
    });

    // The receiver outlives a panic of the consumer task, which is restarted.
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let cleanup_app = app.clone();
    crate::task_supervision::spawn_supervised("cloud_chat_session_cleanup", true, move || {
        let rx = rx.clone();
        let app = cleanup_app.clone();
        async move {
            let mut rx = rx.lock().await;
            while let Some(left) = rx.recv().await {
                let gate = &state().gate;
                {
                    let _held = gate.gate.lock().await;
                    run_cleanup(&app, &left).await;
                }
                gate.finish_cleanup();
            }
        }
    });

    let snapshot_app = app.clone();
    crate::task_supervision::spawn_supervised("cloud_chat_cookie_snapshot", true, move || {
        let app = snapshot_app.clone();
        async move {
            let mut ticker = tokio::time::interval(SNAPSHOT_INTERVAL);
            ticker.tick().await; // the first tick is immediate
            loop {
                ticker.tick().await;
                if !open_ids().is_empty() {
                    crate::commands::cloud_chat_pane::snapshot_open_panes(&app).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(60);

    #[tokio::test]
    async fn an_idle_gate_hands_out_a_current_guard() {
        let gate = JarGate::new();
        let guard = gate.acquire_within(SHORT).await.expect("idle gate");
        assert!(guard.still_current());
    }

    #[tokio::test]
    async fn a_pending_cleanup_blocks_acquire_until_it_finishes() {
        let gate = Arc::new(JarGate::new());
        gate.note_leave();
        assert!(
            gate.acquire_within(SHORT).await.is_none(),
            "refused while pending"
        );

        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.acquire_within(Duration::from_secs(5)).await.is_some() })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "still waiting for the cleanup");
        gate.finish_cleanup();
        assert!(waiter.await.unwrap(), "released once the cleanup finished");
    }

    #[tokio::test]
    async fn a_guard_taken_before_a_leave_is_stale_and_a_later_one_is_current() {
        let gate = JarGate::new();
        let old = gate.acquire_within(SHORT).await.unwrap();
        gate.note_leave();
        assert!(!old.still_current(), "a key left while the guard was held");
        drop(old);
        gate.finish_cleanup();
        let fresh = gate.acquire_within(SHORT).await.unwrap();
        assert!(fresh.still_current());
    }

    #[tokio::test]
    async fn the_gate_serialises_holders() {
        let gate = JarGate::new();
        let first = gate.acquire_within(SHORT).await.unwrap();
        assert!(
            gate.acquire_within(SHORT).await.is_none(),
            "second holder times out"
        );
        drop(first);
        assert!(gate.acquire_within(SHORT).await.is_some());
    }

    #[tokio::test]
    async fn several_pending_cleanups_all_have_to_finish() {
        let gate = JarGate::new();
        gate.note_leave();
        gate.note_leave();
        assert_eq!(gate.pending(), 2);
        gate.finish_cleanup();
        assert!(
            gate.acquire_within(SHORT).await.is_none(),
            "one is still pending"
        );
        gate.finish_cleanup();
        assert_eq!(gate.pending(), 0);
        assert!(gate.acquire_within(SHORT).await.is_some());
    }

    #[tokio::test]
    async fn a_leave_during_the_wait_for_the_gate_is_not_missed() {
        let gate = Arc::new(JarGate::new());
        let holder = gate.acquire_within(SHORT).await.unwrap();
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.acquire_within(Duration::from_millis(400))
                    .await
                    .is_some()
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.note_leave();
        drop(holder);
        // The waiter wins the gate, sees the pending cleanup and must not proceed.
        assert!(!waiter.await.unwrap());
    }

    #[test]
    fn the_open_pane_mirror_tracks_opens_and_closes() {
        for id in ["hygiene-test-a", "hygiene-test-b"] {
            note_closed(id);
        }
        note_open("hygiene-test-a");
        note_open("hygiene-test-a");
        note_open("hygiene-test-b");
        let ids = open_ids();
        assert!(
            ids.contains(&"hygiene-test-a".to_owned())
                && ids.contains(&"hygiene-test-b".to_owned())
        );
        note_closed("hygiene-test-a");
        assert!(!open_ids().contains(&"hygiene-test-a".to_owned()));
        note_closed("hygiene-test-b");
    }

    #[test]
    fn the_snapshot_interval_is_not_a_busy_loop() {
        assert!(SNAPSHOT_INTERVAL >= Duration::from_secs(10));
    }
}

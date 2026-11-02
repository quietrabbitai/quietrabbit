//! Debug-only diagnostics for the two still-open Cloud Chat pane investigations
//! (items.id=649). Mounted in `mod.rs` under `#[cfg(debug_assertions)]`, so a
//! release build compiles none of this and none of its call sites.
//!
//! - items.id=365 (Duck.ai first-click glitch, deferred): why a pane's first
//!   click or key was silently dropped.
//! - items.id=486 (resize-desync race, deferred): `sync_pane_sizes`'
//!   `last_applied_size` cache hits and applies, with a shared sequence number
//!   and millisecond clock so they can be ordered across handlers.
//!
//! Opt-in: `QR_CEF_PANE_DIAG` (debug builds only, separate from
//! `QR_CEF_GPU_DIAG`, which turns on Chromium's own verbose stderr logging).
//! `1` or `all` turns everything on; otherwise a comma-separated topic list
//! (`input`, `resize`). Anything else, or unset, is off.
//!
//! Every `DIAG` log string in the crate lives in this file, and every call
//! into it from another file is preceded by `#[cfg(debug_assertions)]`;
//! `tests/diag_gate.rs` enforces both.

use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};

/// Environment variable that turns these diagnostics on in a debug build.
pub(crate) const PANE_DIAG_ENV: &str = "QR_CEF_PANE_DIAG";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Topic {
    /// items.id=365
    Input,
    /// items.id=486
    Resize,
}

impl Topic {
    fn name(self) -> &'static str {
        match self {
            Topic::Input => "input",
            Topic::Resize => "resize",
        }
    }
}

fn requested(value: Option<&OsStr>, topic: Topic) -> bool {
    let Some(value) = value.and_then(OsStr::to_str) else {
        return false;
    };
    value == "1" || value == "all" || value.split(',').any(|t| t == topic.name())
}

fn enabled(topic: Topic) -> bool {
    static VALUE: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
    let value = VALUE.get_or_init(|| std::env::var_os(PANE_DIAG_ENV));
    requested(value.as_deref(), topic)
}

static DIAG_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
static DIAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Shared monotonic sequence number and elapsed milliseconds.
fn tick() -> (u64, u128) {
    (
        DIAG_SEQ.fetch_add(1, Ordering::Relaxed),
        DIAG_START.elapsed().as_millis(),
    )
}

/// items.id=365: logs why an input event is about to be dropped. `applies`
/// limits it to the event kind the investigation watched (mouse press, raw key
/// down); `probe` is only run when the topic is on and returns
/// `(pane_open, browser_ready, host_present)`.
pub(crate) fn input_dropped(
    kind: &str,
    applies: bool,
    key: &str,
    probe: impl FnOnce() -> (bool, bool, bool),
) {
    if !applies || !enabled(Topic::Input) {
        return;
    }
    match probe() {
        (false, _, _) => {
            log::warn!("DIAG items.id=365: {kind} dropped, pane={key} not open")
        }
        (true, false, _) => {
            log::warn!("DIAG items.id=365: {kind} dropped, pane={key} browser not Ready yet")
        }
        (true, true, false) => log::warn!(
            "DIAG items.id=365: {kind} dropped, pane={key} browser Ready but host() is None"
        ),
        (true, true, true) => {}
    }
}

/// items.id=486: `sync_pane_sizes` skipped a pane because its size equals the
/// cached `last_applied_size`.
pub(crate) fn sync_cache_hit(key: &str, size_px: (u32, u32), open_pane_count: usize) {
    if !enabled(Topic::Resize) {
        return;
    }
    let (seq, elapsed_ms) = tick();
    log::debug!(
        "DIAG items.id=486: seq={seq} t={elapsed_ms}ms sync_pane_sizes \
         CACHE-HIT key={key:?} size_px={size_px:?} (== last_applied_size) \
         open_pane_count={open_pane_count}"
    );
}

/// items.id=486: `sync_pane_sizes` is about to apply `new`, logged before the
/// write so old vs. new is visible.
pub(crate) fn sync_apply(key: &str, old: Option<(u32, u32)>, new: (u32, u32)) {
    if !enabled(Topic::Resize) {
        return;
    }
    let (seq, elapsed_ms) = tick();
    log::debug!(
        "DIAG items.id=486: seq={seq} t={elapsed_ms}ms sync_pane_sizes \
         APPLY key={key:?} old={old:?} new={new:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(v: &str, topic: Topic) -> bool {
        requested(Some(OsStr::new(v)), topic)
    }

    #[test]
    fn unset_is_off() {
        assert!(!requested(None, Topic::Input));
        assert!(!requested(None, Topic::Resize));
    }

    #[test]
    fn one_and_all_turn_every_topic_on() {
        for v in ["1", "all"] {
            assert!(on(v, Topic::Input));
            assert!(on(v, Topic::Resize));
        }
    }

    #[test]
    fn a_topic_list_turns_on_only_the_named_topics() {
        assert!(on("input", Topic::Input));
        assert!(!on("input", Topic::Resize));
        assert!(on("resize", Topic::Resize));
        assert!(!on("resize", Topic::Input));
        assert!(on("input,resize", Topic::Input));
        assert!(on("input,resize", Topic::Resize));
    }

    #[test]
    fn anything_else_is_off() {
        for v in ["", "0", "true", "yes", "INPUT", " input", "inputs", "in"] {
            assert!(!on(v, Topic::Input), "{v:?} must not enable input");
            assert!(!on(v, Topic::Resize), "{v:?} must not enable resize");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_value_is_off() {
        use std::os::unix::ffi::OsStrExt;
        assert!(!requested(
            Some(OsStr::from_bytes(&[0xff, 0xfe])),
            Topic::Input
        ));
    }

    #[test]
    fn tick_is_monotonic() {
        let (a, _) = tick();
        let (b, _) = tick();
        assert!(b > a);
    }
}

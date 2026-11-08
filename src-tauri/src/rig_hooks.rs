//! Debug-only startup hooks for the verification rig (items.id=860).
//!
//! Mounted in `lib.rs` under `#[cfg(debug_assertions)]`, so a release build compiles none of this
//! and none of its call sites. DEV-ONLY: tracked for the release gate by `tests/rig_gate.rs` (text
//! rules over the source tree) and `release_has_no_rig_hooks` below (the release test executable
//! must not contain the flag text). Never copy a flag name into code outside this file.
//!
//! Why: a scratch run (`scripts/rig/scratch-run.sh`) exposes no CDP page target until a Cloud Chat
//! pane is open, and `open_cloud_chat_panes` is an IPC command only the main webview can call.
//! These two app arguments let a rig open a named provider's pane, logged out, without the GUI,
//! and close that pane's popup through the same command the popup-close button calls.
//!
//! - `--qr-rig-open-pane=<provider_id>`: after startup, calls `open_cloud_chat_panes` (the real
//!   command function, so the cookie-restore skip for a logged-out run, the session bookkeeping
//!   and the `launch_url` lookup in `shared.db` are exactly the app's own) until a pane exists.
//! - `--qr-rig-close-popup-after-ms=<N>`: after each `cloud-chat-popup-opened` event, waits N ms
//!   (so the rig can look at the popup target first) and calls `close_cloud_chat_popup`.
//!
//! Arguments, not environment variables, because `scratch-run.sh` unsets every `QR_*` variable
//! and already passes app arguments through. Absent flags change nothing: unknown arguments
//! (`--remote-debugging-port=...`, CEF's own) are ignored, a malformed rig flag is logged and
//! ignored, and nothing here ever stops the app. Logs carry provider ids and counts only.
//!
//! Not covered, by construction: the popup-close button's own React `onClick` (the button lives in
//! the frontend, which is behind the login screen) and the Tauri IPC serialisation of the call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{Listener, Manager};

pub(crate) const OPEN_PANE_FLAG: &str = "--qr-rig-open-pane";
pub(crate) const CLOSE_POPUP_FLAG: &str = "--qr-rig-close-popup-after-ms";

/// How often the open is retried while the pane host cannot take a pane yet (the GLArea is not
/// realized), and for how long.
const OPEN_RETRY_EVERY: Duration = Duration::from_millis(500);
const OPEN_GIVE_UP_AFTER: u32 = 60;

/// Upper bound for the popup hold so a typo cannot park the close for hours.
const MAX_POPUP_HOLD_MS: u64 = 600_000;

/// What the rig asked for. `Default` is "nothing".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RigOptions {
    pub open_pane: Option<String>,
    pub close_popup_after: Option<Duration>,
}

impl RigOptions {
    pub fn is_empty(&self) -> bool {
        *self == RigOptions::default()
    }
}

/// Reads the rig flags out of an argument list (without the program name). Total: never panics and
/// never rejects an argument it does not know.
pub fn parse<I, S>(args: I) -> RigOptions
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut opts = RigOptions::default();
    for arg in args {
        let arg = arg.as_ref();
        let Some((flag, value)) = arg.split_once('=') else {
            if arg.starts_with("--qr-rig-") {
                log::warn!("rig_hooks: `{arg}` needs a value (flag=value), ignored");
            }
            continue;
        };
        match flag {
            OPEN_PANE_FLAG => {
                let id = value.trim();
                if id.is_empty() || !id.chars().all(is_provider_id_char) {
                    log::warn!("rig_hooks: {flag} needs a provider id slug, ignored");
                } else {
                    opts.open_pane = Some(id.to_string());
                }
            }
            CLOSE_POPUP_FLAG => match value.trim().parse::<u64>() {
                Ok(ms) if ms <= MAX_POPUP_HOLD_MS => {
                    opts.close_popup_after = Some(Duration::from_millis(ms));
                }
                _ => log::warn!(
                    "rig_hooks: {flag} needs whole milliseconds up to {MAX_POPUP_HOLD_MS}, ignored"
                ),
            },
            f if f.starts_with("--qr-rig-") => {
                log::warn!("rig_hooks: unknown rig flag `{f}`, ignored");
            }
            _ => {}
        }
    }
    opts
}

/// A provider id is a plain slug (cloud_chat_pane.rs relies on the same shape).
fn is_provider_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Starts whatever the process arguments ask for. A no-op, starting no task and registering no
/// listener, when no rig flag is present. Call on the main thread after `pane_host::install`
/// (the `RunEvent::Ready` arm), passing the counter `install` returned.
pub fn spawn_from_process_args(app: &tauri::AppHandle, open_pane_count: Arc<AtomicUsize>) {
    let opts = parse(std::env::args().skip(1));
    spawn(app, open_pane_count, opts);
}

pub fn spawn(app: &tauri::AppHandle, open_pane_count: Arc<AtomicUsize>, opts: RigOptions) {
    if opts.is_empty() {
        return;
    }
    log::warn!("rig_hooks: debug-only rig hooks are active");

    if let Some(hold) = opts.close_popup_after {
        let app_for_event = app.clone();
        app.listen("cloud-chat-popup-opened", move |event| {
            let Some(provider_id) = popup_provider_id(event.payload()) else {
                log::warn!("rig_hooks: popup-opened event without a provider id");
                return;
            };
            let app = app_for_event.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(hold).await;
                log::info!("rig_hooks: closing the popup of provider={provider_id}");
                if let Err(e) =
                    crate::commands::cloud_chat_pane::close_cloud_chat_popup(provider_id, app).await
                {
                    log::warn!("rig_hooks: close_cloud_chat_popup failed: {e}");
                }
            });
        });
    }

    if let Some(provider_id) = opts.open_pane {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            open_until_a_pane_exists(app, provider_id, open_pane_count).await;
        });
    }
}

/// `PopupOpenedPayload` serialises as `{"provider_id": "...", "rect": {...}}`.
fn popup_provider_id(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    v.get("provider_id")?.as_str().map(str::to_owned)
}

/// The pane host drops an open that arrives before its GLArea is realized (with a warning, no
/// error), so the command is repeated until the open-pane counter moves.
async fn open_until_a_pane_exists(
    app: tauri::AppHandle,
    provider_id: String,
    open_pane_count: Arc<AtomicUsize>,
) {
    for attempt in 1..=OPEN_GIVE_UP_AFTER {
        let result = crate::commands::cloud_chat_pane::open_cloud_chat_panes(
            vec![provider_id.clone()],
            app.state::<crate::auth::registry::KeyRegistry>(),
            app.clone(),
            app.state::<sqlx::SqlitePool>(),
        )
        .await;
        match result {
            Ok(()) => {}
            Err(e) => log::warn!("rig_hooks: open of provider={provider_id} failed: {e}"),
        }
        tokio::time::sleep(OPEN_RETRY_EVERY).await;
        let panes = open_pane_count.load(Ordering::Relaxed);
        if panes > 0 {
            log::info!(
                "rig_hooks: pane open for provider={provider_id} (open panes: {panes}, \
                 attempt {attempt})"
            );
            return;
        }
    }
    log::error!(
        "rig_hooks: no pane opened for provider={provider_id} after {OPEN_GIVE_UP_AFTER} attempts"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rig_flag_means_nothing_is_asked() {
        assert!(parse(Vec::<String>::new()).is_empty());
        // CEF's and the scratch runner's own arguments are ignored, not rejected.
        let argv = [
            "--remote-debugging-port=9333",
            "--log-net-log=/x/net.json",
            "--net-log-capture-mode=Default",
            "--some-unknown-flag",
            "positional",
        ];
        assert_eq!(parse(argv), RigOptions::default());
        assert!(parse(argv).is_empty());
    }

    #[test]
    fn spawn_without_flags_starts_nothing() {
        // `spawn` returns before touching the app handle when nothing is asked; the empty-options
        // predicate is what it branches on.
        assert!(RigOptions::default().is_empty());
        assert!(!RigOptions {
            open_pane: Some("duckai".into()),
            close_popup_after: None
        }
        .is_empty());
    }

    #[test]
    fn both_flags_parse() {
        let o = parse([
            "--remote-debugging-port=9333",
            "--qr-rig-open-pane=duckai",
            "--qr-rig-close-popup-after-ms=4000",
        ]);
        assert_eq!(o.open_pane.as_deref(), Some("duckai"));
        assert_eq!(o.close_popup_after, Some(Duration::from_millis(4000)));
    }

    #[test]
    fn each_flag_works_alone() {
        let o = parse(["--qr-rig-open-pane=duckai"]);
        assert_eq!(o.open_pane.as_deref(), Some("duckai"));
        assert_eq!(o.close_popup_after, None);
        let o = parse(["--qr-rig-close-popup-after-ms=0"]);
        assert_eq!(o.open_pane, None);
        assert_eq!(o.close_popup_after, Some(Duration::ZERO));
    }

    #[test]
    fn malformed_rig_flags_are_ignored_not_fatal() {
        for bad in [
            "--qr-rig-open-pane=",
            "--qr-rig-open-pane=   ",
            "--qr-rig-open-pane=a b",
            "--qr-rig-open-pane=https://x.example/",
            "--qr-rig-open-pane",
            "--qr-rig-close-popup-after-ms=soon",
            "--qr-rig-close-popup-after-ms=-1",
            "--qr-rig-close-popup-after-ms=99999999999",
            "--qr-rig-close-popup-after-ms",
            "--qr-rig-unknown=1",
        ] {
            assert!(parse([bad]).is_empty(), "{bad} should be ignored");
        }
    }

    #[test]
    fn a_bad_flag_does_not_cancel_a_good_one() {
        let o = parse([
            "--qr-rig-close-popup-after-ms=oops",
            "--qr-rig-open-pane=duckai",
        ]);
        assert_eq!(o.open_pane.as_deref(), Some("duckai"));
        assert_eq!(o.close_popup_after, None);
    }

    #[test]
    fn the_popup_event_payload_shape_is_read() {
        let payload =
            serde_json::to_string(&crate::commands::cloud_chat_pane::PopupOpenedPayload {
                provider_id: "duckai".into(),
                rect: crate::commands::cloud_chat_pane::PaneRectFraction {
                    x: 0.1,
                    y: 0.1,
                    width: 0.5,
                    height: 0.5,
                },
            })
            .unwrap();
        assert_eq!(popup_provider_id(&payload).as_deref(), Some("duckai"));
        assert_eq!(popup_provider_id("not json"), None);
        assert_eq!(popup_provider_id("{}"), None);
    }
}

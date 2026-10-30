// src-tauri/src/write_failure.rs
//
// No silent loss (items.id=778, decisions.id=896 point 8). A write of the
// user's own content that fails must reach the user: a "non-fatal" log line
// that drops a reply, a stopped partial or the final status of a run is not
// acceptable. Three parts, shared by every such site:
//
//   - `retry_transient`: a bounded retry (3 attempts, 200 ms then 400 ms) for
//     an ordinary transient failure such as a busy file. A refusal that
//     retrying cannot fix (the key was refused, the epoch changed, the file no
//     longer opens under this key) is returned at once.
//   - `report`: an error log and the content-free `data-write-failed` event
//     (`{ "kind": "reply" | "run" | "history" }`: never any text, id or path)
//     that the webview turns into a visible notice.
//   - `is_key_refusal`: tells the two apart from the error text.
//
// The writer lease (auth::writer_gate) keeps a rotation from causing these;
// what is left is another session rotating the key, a full disk, a damaged
// file. Those must not be silent either.

use std::time::Duration;

use serde::Serialize;
use tauri::Emitter;

pub const WRITE_FAILED_EVENT: &str = "data-write-failed";

/// What could not be saved. A fixed, content-free vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    /// A chat reply (its text, or its stopped partial).
    Reply,
    /// The final status of a Focus run.
    Run,
    /// The History entry of a chat.
    History,
    /// A reply's outcome marker (complete or cut off).
    ReplyStatus,
    /// The flag that marks a reply as containing personal details.
    PersonalFlag,
    /// The text read from an imported image.
    OcrText,
    /// The searchable text of an imported PDF or Word document (items.id=837).
    ExtractedText,
}

impl Lost {
    pub fn as_str(self) -> &'static str {
        match self {
            Lost::Reply => "reply",
            Lost::Run => "run",
            Lost::History => "history",
            Lost::ReplyStatus => "reply_status",
            Lost::PersonalFlag => "personal_flag",
            Lost::OcrText => "ocr_text",
            Lost::ExtractedText => "extracted_text",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct WriteFailedPayload {
    kind: &'static str,
}

/// Attempts at a transient write failure (the first included) and the pauses
/// between them.
pub const WRITE_ATTEMPTS: usize = 3;
pub const WRITE_BACKOFF: [Duration; WRITE_ATTEMPTS - 1] =
    [Duration::from_millis(200), Duration::from_millis(400)];

/// Whether an error text says the account's key was refused or has changed (a
/// retry cannot help): a rotation under way, another session's key change, no
/// key epoch to derive under, or a file that no longer opens under this key.
pub fn is_key_refusal(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("data key is being changed")
        || m.contains("changed in another session")
        || m.contains("could not check the account key epoch")
        || m.contains("no key epoch is registered")
        || m.contains("file is not a database")
        || m.contains("hmac")
}

/// Run `op`, retrying a transient failure up to `WRITE_ATTEMPTS` times. A key
/// refusal is returned on the first attempt.
pub async fn retry_transient<T, E, F, Fut>(mut op: F) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    retry_with(&WRITE_BACKOFF, &mut op).await
}

async fn retry_with<T, E, F, Fut>(backoff: &[Duration], op: &mut F) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < backoff.len() && !is_key_refusal(&e.to_string()) => {
                tokio::time::sleep(backoff[attempt]).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Make a failed write visible: log it and tell the webview (content-free).
/// `handle` is None where there is no webview (tests, a background sweep); the
/// log line then stands, at error level.
pub fn report<R: tauri::Runtime>(
    handle: Option<&impl Emitter<R>>,
    lost: Lost,
    detail: &dyn std::fmt::Display,
) {
    log::error!(
        "write failed, user content not saved ({}): {detail}",
        lost.as_str()
    );
    if let Some(handle) = handle {
        if let Err(e) = handle.emit(
            WRITE_FAILED_EVENT,
            WriteFailedPayload {
                kind: lost.as_str(),
            },
        ) {
            log::warn!("write_failure: couldn't emit {WRITE_FAILED_EVENT}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_key_refusal_is_told_apart_from_a_busy_file() {
        assert!(is_key_refusal(
            "the account's data key is being changed, try again in a moment"
        ));
        assert!(is_key_refusal(
            "this account's key was changed in another session, unlock again"
        ));
        assert!(is_key_refusal("file is not a database (code 26)"));
        assert!(is_key_refusal(
            "Migration error: no key epoch is registered for user 'u'"
        ));
        assert!(!is_key_refusal("database is locked"));
        assert!(!is_key_refusal("disk I/O error"));
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_then_succeeds() {
        let calls = AtomicUsize::new(0);
        let r: Result<u8, String> = retry_with(&[Duration::ZERO, Duration::ZERO], &mut || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err("database is locked".to_owned())
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_retries_are_bounded() {
        let calls = AtomicUsize::new(0);
        let r: Result<u8, String> = retry_with(&[Duration::ZERO, Duration::ZERO], &mut || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("database is locked".to_owned())
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), WRITE_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_key_refusal_is_not_retried() {
        let calls = AtomicUsize::new(0);
        let r: Result<u8, String> = retry_with(&[Duration::ZERO, Duration::ZERO], &mut || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("file is not a database (code 26)".to_owned())
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_write_reaches_the_webview_as_a_content_free_event() {
        use tauri::Listener;
        let app = tauri::test::mock_app();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&seen);
        app.handle().listen(WRITE_FAILED_EVENT, move |event| {
            sink.lock().unwrap().push(event.payload().to_owned());
        });
        report(
            Some(app.handle()),
            Lost::Reply,
            &"file is not a database (code 26) /secret/path user text",
        );
        // The event carries the kind and nothing from the error.
        assert_eq!(seen.lock().unwrap().as_slice(), [r#"{"kind":"reply"}"#]);
    }

    #[test]
    fn every_kind_has_its_own_content_free_name() {
        let kinds = [
            Lost::Reply,
            Lost::Run,
            Lost::History,
            Lost::ReplyStatus,
            Lost::PersonalFlag,
            Lost::OcrText,
            Lost::ExtractedText,
        ];
        let names: std::collections::BTreeSet<&str> = kinds.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), kinds.len(), "no two kinds share a name");
        for n in names {
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{n}: a fixed vocabulary, never text"
            );
        }
    }

    /// The sites that used to log and drop user content now report. A source
    /// scan, so a later edit cannot quietly turn one back into a warning.
    #[test]
    fn the_covered_write_sites_report_their_failures() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let read = |f: &str| std::fs::read_to_string(src.join(f)).unwrap();
        let exec = read("conductor/executor.rs");
        let ingest = read("commands/ingest.rs");
        let messages = read("commands/messages.rs");
        let lifecycle = read("conductor/lifecycle.rs");
        for (name, text, kind) in [
            ("record_completed_reply", &exec, "Lost::ReplyStatus"),
            ("reply_gate2", &exec, "Lost::PersonalFlag"),
            ("keep_partial_reply", &exec, "Lost::Reply"),
            ("store_ingested_document", &ingest, "Lost::OcrText"),
            ("store_ingested_document", &ingest, "Lost::ExtractedText"),
            ("finalize_chat_reply", &messages, "Lost::History"),
            ("write_focus_run_record_logged", &lifecycle, "Lost::Run"),
        ] {
            let start = text
                .find(&format!("fn {name}"))
                .unwrap_or_else(|| panic!("{name}"));
            let end = text[start..]
                .find("\n}\n")
                .map_or(text.len(), |i| start + i);
            let body = &text[start..end];
            assert!(
                body.contains("write_failure::report") && body.contains(kind),
                "{name} must report a failed write as {kind}"
            );
        }
    }

    #[test]
    fn the_event_carries_only_a_kind() {
        let json = serde_json::to_string(&WriteFailedPayload {
            kind: Lost::Reply.as_str(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"reply"}"#);
    }
}

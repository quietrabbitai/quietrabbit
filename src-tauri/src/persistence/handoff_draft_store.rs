// src-tauri/src/persistence/handoff_draft_store.rs
//
// items.id=387 (decisions.id=942 point 7, S1): the store of a PASTED HAND-OFF
// REPLY, the text a person copies back from a provider they used by hand.
//
// WHAT IT IS. An ingested output (source external_ingested, the Library's
// "Imported" view) of output_type `handoff_draft`, stored through the same three
// writes as any imported document (persistence::ingested_document_store), whose
// focus_slug is the Focus that took the paste. The text is UNTRUSTED EXTERNAL
// CONTENT: it came from outside, was never produced by QR, and is read back only
// through this module, by an opaque H- handle (persistence::handles), as exact
// text. Nothing here parses it, rewrites it or feeds it to a model.
//
// WHAT IT IS NOT. It is not chat scratch. It is a durable Library document the
// user owns and removes through the Library, so a chat clear or the session
// store's retention sweep (items.id=888) does not delete it. The two session
// variables that link a chat to it (handoff_pending, handoff_received, S2) live
// in session_variables and are covered by that sweep.
//
// CAPS. PASTE_MAX_BYTES (256 KiB) bounds a stored draft: it is far below the
// Library's 100 MiB file cap on purpose, because a draft is read back into local
// prompts. A paste must be non-blank after trimming and carry no NUL (UTF-8 is
// inherent in a Rust String). The chat turn that carried the paste has no byte
// cap of its own (commands::messages), so this bounds the stored draft, not the
// chat window.
//
// NO LEASE here: the caller inside a run holds the run's writer lease.
//
// NEVER LOGGED. DraftText's Debug redacts the text and no error carries it.
// READS ONLY, apart from store_draft's one ingest. Nothing in this module
// updates or deletes an output (a source scan pins it): deletion is the
// Library's soft delete.

use sqlx::{Row, SqliteConnection};

use crate::persistence::handles::{self, HandleError};
use crate::persistence::ingested_document_store::persist_ingested_document;

/// The output_type of a stored hand-off reply (free text in the schema).
pub const OUTPUT_TYPE: &str = "handoff_draft";

/// Largest paste a draft may hold, in bytes. Chat-PM/Jason approved 256 KiB
/// (items.id=387 plan Q2).
pub const PASTE_MAX_BYTES: usize = 262_144;

/// The sensitivity a stored reply carries. Jason approved `personal` (plan Q1):
/// the fail-safe direction, since the paste is about the user's own work.
pub const SENSITIVITY: &str = "personal";

/// The original_filename recorded for a pasted reply.
pub const ORIGINAL_FILENAME: &str = "handoff-reply.txt";

const FOCUS_ID_MAX_CHARS: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    #[error("the pasted reply is empty")]
    Blank,
    #[error("the pasted reply is larger than the {PASTE_MAX_BYTES} byte limit")]
    TooLarge,
    #[error("the pasted reply contains a character that cannot be stored")]
    ContainsNul,
    #[error("the focus id is not usable")]
    InvalidFocus,
    #[error("the data key is not usable")]
    BadKey,
    #[error("no such hand-off reply for this Focus")]
    NotFound,
    #[error("the hand-off reply could not be stored: {0}")]
    Store(String),
    #[error("{0}")]
    Handle(#[from] HandleError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The exact text of a stored reply. Debug redacts it.
pub struct DraftText(String);

impl DraftText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for DraftText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DraftText(<redacted, {} bytes>)", self.0.len())
    }
}

/// What a paste must satisfy to be stored. Pure.
pub fn validate_paste(text: &str) -> Result<(), DraftError> {
    if text.contains('\0') {
        return Err(DraftError::ContainsNul);
    }
    if text.trim().is_empty() {
        return Err(DraftError::Blank);
    }
    if text.len() > PASTE_MAX_BYTES {
        return Err(DraftError::TooLarge);
    }
    Ok(())
}

/// Store a pasted reply for `focus_id` and return its output id. Validates first
/// (nothing is written for a rejected paste). `data_key_hex` is the session's
/// data key in the hex form the openers take.
pub async fn store_draft(
    user_id: &str,
    persona_id: &str,
    data_key_hex: &str,
    focus_id: &str,
    text: &str,
) -> Result<String, DraftError> {
    validate_paste(text)?;
    if focus_id.is_empty() || focus_id.chars().count() > FOCUS_ID_MAX_CHARS {
        return Err(DraftError::InvalidFocus);
    }
    let master =
        crate::auth::subkeys::decode_dek_hex(data_key_hex).map_err(|_| DraftError::BadKey)?;
    let output_id = uuid::Uuid::new_v4().to_string();
    persist_ingested_document(
        user_id,
        persona_id,
        &master,
        data_key_hex,
        &output_id,
        OUTPUT_TYPE,
        focus_id,
        None,
        SENSITIVITY,
        ORIGINAL_FILENAME,
        text.as_bytes(),
        Some(text),
    )
    .await
    .map_err(DraftError::Store)?;
    Ok(output_id)
}

/// The ids of this Focus's stored replies that are not deleted, oldest first.
/// Only outputs of the exact type, from the Library's imports, filed under the
/// Focus: another Focus's, a QR-generated output, any other imported document and
/// a soft-deleted reply are all absent.
pub async fn list_draft_ids(
    conn: &mut SqliteConnection,
    focus_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM outputs
         WHERE source = 'external_ingested' AND output_type = ?
           AND focus_slug = ? AND deleted_at IS NULL
         ORDER BY created_at ASC, id ASC",
    )
    .bind(OUTPUT_TYPE)
    .bind(focus_id)
    .fetch_all(&mut *conn)
    .await
}

/// The exact text of one stored reply, by output id, scoped to the Focus.
pub async fn read_draft(
    conn: &mut SqliteConnection,
    focus_id: &str,
    output_id: &str,
) -> Result<DraftText, DraftError> {
    let row = sqlx::query(
        "SELECT content FROM outputs
         WHERE id = ? AND source = 'external_ingested' AND output_type = ?
           AND focus_slug = ? AND deleted_at IS NULL",
    )
    .bind(output_id)
    .bind(OUTPUT_TYPE)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DraftError::NotFound)?;
    let content: Option<String> = row.try_get("content")?;
    content.map(DraftText).ok_or(DraftError::NotFound)
}

/// The exact text behind a draft handle: the one way a pass reads a reply.
pub async fn read_draft_by_handle(
    conn: &mut SqliteConnection,
    focus_id: &str,
    handle: &str,
) -> Result<DraftText, DraftError> {
    let id = handles::resolve_draft_handle(conn, focus_id, handle).await?;
    read_draft(conn, focus_id, &id).await
}

/// Facts about a stored reply that are safe to show without its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftInfo {
    pub byte_count: usize,
    pub created_at: String,
}

/// Size and time of one stored reply, by output id, scoped to the Focus.
pub async fn draft_info(
    conn: &mut SqliteConnection,
    focus_id: &str,
    output_id: &str,
) -> Result<DraftInfo, DraftError> {
    let row = sqlx::query(
        "SELECT length(CAST(content AS BLOB)) AS n, created_at FROM outputs
         WHERE id = ? AND source = 'external_ingested' AND output_type = ?
           AND focus_slug = ? AND deleted_at IS NULL",
    )
    .bind(output_id)
    .bind(OUTPUT_TYPE)
    .bind(focus_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DraftError::NotFound)?;
    let n: Option<i64> = row.try_get("n")?;
    Ok(DraftInfo {
        byte_count: usize::try_from(n.unwrap_or(0)).unwrap_or(0),
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::persistence::handles::{draft_handle, resolve_draft_handle};
    use crate::persistence::ingest_blob;
    use crate::persistence::ingested_document_store::INGESTED_DOCUMENT_OUTPUT_TYPE;
    use crate::persistence::output_store;

    const USER: &str = "handoff-387-user";
    const PERSONA: &str = "handoff-387-persona";
    const FOCUS: &str = "writing-assistant";
    const KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] = [0x37u8; crate::auth::kdf::MASTER_KEY_LEN];

    /// QR_DATA_ROOT at a fresh tempdir for one test.
    struct DataRoot {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl DataRoot {
        async fn new() -> Self {
            let lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().expect("tempdir");
            std::env::set_var("QR_DATA_ROOT", dir.path());
            DataRoot {
                _lock: lock,
                saved,
                _dir: dir,
            }
        }
    }

    impl Drop for DataRoot {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    fn kh() -> String {
        key_hex(&KEY)
    }

    async fn conn() -> SqliteConnection {
        output_store::open_outputs_db(USER, PERSONA, &kh())
            .await
            .expect("outputs.db")
    }

    async fn store(focus: &str, text: &str) -> String {
        store_draft(USER, PERSONA, &kh(), focus, text)
            .await
            .expect("store_draft")
    }

    // -- the paste rules -------------------------------------------------------

    #[test]
    fn a_paste_must_be_non_blank_nul_free_and_within_the_cap() {
        assert!(validate_paste("a reply").is_ok());
        assert!(matches!(validate_paste(""), Err(DraftError::Blank)));
        assert!(matches!(validate_paste(" \n\t "), Err(DraftError::Blank)));
        assert!(matches!(
            validate_paste("a\0b"),
            Err(DraftError::ContainsNul)
        ));
        assert!(validate_paste(&"a".repeat(PASTE_MAX_BYTES)).is_ok());
        assert!(matches!(
            validate_paste(&"a".repeat(PASTE_MAX_BYTES + 1)),
            Err(DraftError::TooLarge)
        ));
        // The cap counts bytes, not characters: 3-byte characters hit it early.
        let wide = "€".repeat(PASTE_MAX_BYTES / 3 + 1);
        assert!(wide.chars().count() < PASTE_MAX_BYTES);
        assert!(matches!(validate_paste(&wide), Err(DraftError::TooLarge)));
        // A NUL wins over a blank or oversize reading, so the reason is stable.
        assert!(matches!(
            validate_paste(&format!("\0{}", " ".repeat(PASTE_MAX_BYTES + 5))),
            Err(DraftError::ContainsNul)
        ));
    }

    #[test]
    fn the_constants_are_the_approved_ones() {
        assert_eq!(PASTE_MAX_BYTES, 256 * 1024);
        assert_eq!(SENSITIVITY, "personal");
        assert_eq!(OUTPUT_TYPE, "handoff_draft");
    }

    // -- store and read --------------------------------------------------------

    #[tokio::test]
    async fn a_stored_reply_reads_back_as_the_exact_text() {
        let _root = DataRoot::new().await;
        // Braces, leading and trailing space, a blank line and non-ASCII are all
        // kept: nothing here cleans, trims or escapes the text.
        let text = "  {not a token} Dear team,\n\n   café — ok.\n ";
        let id = store(FOCUS, text).await;

        let mut c = conn().await;
        assert_eq!(read_draft(&mut c, FOCUS, &id).await.unwrap().as_str(), text);

        // The blob is the same bytes.
        let path = ingest_blob::ingest_blob_path(USER, PERSONA, &id, 1);
        let plain = ingest_blob::read_encrypted_blob(&path, &KEY, USER, PERSONA, &id, 1)
            .await
            .unwrap();
        assert_eq!(plain, text.as_bytes());

        // The row is an ingested, personal, reference output of the Focus.
        let rec = output_store::get_output(USER, PERSONA, &kh(), &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.source, "external_ingested");
        assert_eq!(rec.output_type, OUTPUT_TYPE);
        assert_eq!(rec.sensitivity, SENSITIVITY);
        assert_eq!(rec.original_filename.as_deref(), Some(ORIGINAL_FILENAME));

        let info = draft_info(&mut c, FOCUS, &id).await.unwrap();
        assert_eq!(info.byte_count, text.len());
        assert!(!info.created_at.is_empty());
    }

    #[tokio::test]
    async fn a_rejected_paste_writes_nothing() {
        let _root = DataRoot::new().await;
        for bad in ["", "  \n", "a\0b"] {
            assert!(store_draft(USER, PERSONA, &kh(), FOCUS, bad).await.is_err());
        }
        let too_big = "a".repeat(PASTE_MAX_BYTES + 1);
        assert!(matches!(
            store_draft(USER, PERSONA, &kh(), FOCUS, &too_big).await,
            Err(DraftError::TooLarge)
        ));
        assert!(matches!(
            store_draft(USER, PERSONA, &kh(), "", "ok").await,
            Err(DraftError::InvalidFocus)
        ));
        assert!(matches!(
            store_draft(USER, PERSONA, "not-a-key", FOCUS, "ok").await,
            Err(DraftError::BadKey)
        ));
        // No database was even opened, and no blob exists.
        let ingest_root = ingest_blob::ingest_blob_path(USER, PERSONA, "x", 1);
        assert!(!ingest_root.parent().unwrap().exists());
    }

    #[tokio::test]
    async fn a_cap_sized_reply_is_stored() {
        let _root = DataRoot::new().await;
        let text = "a".repeat(PASTE_MAX_BYTES);
        let id = store(FOCUS, &text).await;
        let mut c = conn().await;
        assert_eq!(
            read_draft(&mut c, FOCUS, &id).await.unwrap().as_str().len(),
            PASTE_MAX_BYTES
        );
    }

    // -- handles ---------------------------------------------------------------

    #[tokio::test]
    async fn a_draft_handle_resolves_only_this_focus_own_live_reply() {
        let _root = DataRoot::new().await;
        let mine = store(FOCUS, "mine").await;
        let theirs = store("other-focus", "theirs").await;
        let mut c = conn().await;

        let h = draft_handle(FOCUS, &mine);
        assert!(h.starts_with("H-") && h.len() == 2 + handles::HANDLE_CHARS);
        assert!(!h.contains(&mine), "the output id is not in the handle");
        assert_eq!(resolve_draft_handle(&mut c, FOCUS, &h).await.unwrap(), mine);
        assert_eq!(
            read_draft_by_handle(&mut c, FOCUS, &h)
                .await
                .unwrap()
                .as_str(),
            "mine"
        );

        // Another Focus's reply, under either hash.
        for foreign in [
            draft_handle("other-focus", &theirs),
            draft_handle(FOCUS, &theirs),
        ] {
            assert!(matches!(
                resolve_draft_handle(&mut c, FOCUS, &foreign).await,
                Err(HandleError::NotFound(_))
            ));
            assert!(matches!(
                read_draft_by_handle(&mut c, FOCUS, &foreign).await,
                Err(DraftError::Handle(HandleError::NotFound(_)))
            ));
        }
        // It resolves for the Focus that owns it.
        assert_eq!(
            resolve_draft_handle(&mut c, "other-focus", &draft_handle("other-focus", &theirs))
                .await
                .unwrap(),
            theirs
        );
        // Not a draft handle at all, or another kind's.
        for bad in [
            mine.clone(),
            handles::case_handle(FOCUS, &mine),
            handles::entity_handle(FOCUS, &mine),
        ] {
            assert!(matches!(
                resolve_draft_handle(&mut c, FOCUS, &bad).await,
                Err(HandleError::Malformed('H'))
            ));
        }
        // And a draft handle is not another kind's.
        assert!(matches!(
            handles::resolve_case_handle(&mut c, FOCUS, &h).await,
            Err(HandleError::Malformed('T'))
        ));
        assert!(matches!(
            handles::resolve_delta_handle(&mut c, FOCUS, &h).await,
            Err(HandleError::Malformed('D'))
        ));
    }

    #[tokio::test]
    async fn other_documents_and_deleted_replies_are_out_of_reach() {
        let _root = DataRoot::new().await;
        let draft = store(FOCUS, "a reply").await;

        // An ordinary import filed under the same Focus.
        let imported = "imported-doc";
        let master = KEY;
        persist_ingested_document(
            USER,
            PERSONA,
            &master,
            &kh(),
            imported,
            INGESTED_DOCUMENT_OUTPUT_TYPE,
            FOCUS,
            None,
            "general",
            "doc.txt",
            b"an imported document",
            Some("an imported document"),
        )
        .await
        .unwrap();

        // A QR-generated output with the draft's type and Focus.
        let mut c = conn().await;
        let run = output_store::create_ingest_focus_run(USER, PERSONA, &kh())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO outputs (id, focus_run_id, output_type, content, sensitivity,
                                  status, created_at, updated_at, focus_slug)
             VALUES ('qr-made', ?, ?, 'generated', 'general', 'finalized', 't', 't', ?)",
        )
        .bind(&run)
        .bind(OUTPUT_TYPE)
        .bind(FOCUS)
        .execute(&mut c)
        .await
        .unwrap();

        for id in [imported, "qr-made"] {
            assert!(matches!(
                read_draft(&mut c, FOCUS, id).await,
                Err(DraftError::NotFound)
            ));
            assert!(matches!(
                draft_info(&mut c, FOCUS, id).await,
                Err(DraftError::NotFound)
            ));
            assert!(matches!(
                resolve_draft_handle(&mut c, FOCUS, &draft_handle(FOCUS, id)).await,
                Err(HandleError::NotFound(_))
            ));
        }
        assert_eq!(
            list_draft_ids(&mut c, FOCUS).await.unwrap(),
            vec![draft.clone()]
        );

        // The Library's soft delete takes the reply out of reach.
        output_store::delete_output(USER, PERSONA, &kh(), &draft, None)
            .await
            .unwrap();
        let mut c = conn().await;
        assert!(list_draft_ids(&mut c, FOCUS).await.unwrap().is_empty());
        assert!(matches!(
            read_draft_by_handle(&mut c, FOCUS, &draft_handle(FOCUS, &draft)).await,
            Err(DraftError::Handle(HandleError::NotFound(_)))
        ));
        assert!(matches!(
            read_draft(&mut c, FOCUS, &draft).await,
            Err(DraftError::NotFound)
        ));
    }

    #[tokio::test]
    async fn two_replies_each_have_their_own_handle() {
        let _root = DataRoot::new().await;
        let a = store(FOCUS, "first").await;
        let b = store(FOCUS, "second").await;
        let mut c = conn().await;
        assert_ne!(draft_handle(FOCUS, &a), draft_handle(FOCUS, &b));
        assert_eq!(
            read_draft_by_handle(&mut c, FOCUS, &draft_handle(FOCUS, &a))
                .await
                .unwrap()
                .as_str(),
            "first"
        );
        assert_eq!(
            read_draft_by_handle(&mut c, FOCUS, &draft_handle(FOCUS, &b))
                .await
                .unwrap()
                .as_str(),
            "second"
        );
        assert_eq!(list_draft_ids(&mut c, FOCUS).await.unwrap().len(), 2);
    }

    // -- the text never leaves through Debug or an error -------------------------

    #[test]
    fn debug_and_errors_never_carry_the_text() {
        let secret = "the-secret-reply-text";
        let t = DraftText(secret.to_owned());
        let dbg = format!("{t:?}");
        assert!(!dbg.contains(secret), "{dbg}");
        assert!(dbg.contains("redacted"));
        for e in [
            DraftError::Blank,
            DraftError::TooLarge,
            DraftError::ContainsNul,
            DraftError::InvalidFocus,
            DraftError::BadKey,
            DraftError::NotFound,
            DraftError::Handle(HandleError::NotFound("hand-off reply")),
        ] {
            assert!(!e.to_string().contains(secret));
        }
    }

    // -- source scan: this module never updates or deletes an output ---------------

    #[test]
    fn this_module_has_no_update_or_delete_statement() {
        let src = include_str!("handoff_draft_store.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        // Statements only, not prose: the SQL strings of this module are
        // upper-case.
        for banned in ["UPDATE outputs", "DELETE FROM", "INSERT INTO", "DROP "] {
            assert!(
                !body.contains(banned),
                "handoff_draft_store.rs must not contain '{banned}'"
            );
        }
    }
}

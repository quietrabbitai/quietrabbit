// src-tauri/src/commands/ingest.rs
//
// items.id=383 (decisions.id=486): storage model for user-uploaded/ingested
// documents. Ingested documents live in the same outputs table as
// QR-generated content (source='external_ingested'), backed by a real
// encrypted file on disk (persistence/ingest_blob.rs) rather than the
// outputs.content TEXT column, attached to a lightweight ingest-only
// focus_run (output_store::create_ingest_focus_run) to satisfy
// outputs.focus_run_id's NOT NULL FK.
//
// NOT `ingest_document`: decisions.id=491 (D6-449) and
// HANDOFF_IPC_SURFACE.md's ingestion design section already reserve that
// name for a future command with an `intent` (extract | import_draft)
// parameter -- both intents need machinery this item explicitly excludes
// (extract needs decisions.id=488's LLM extraction/ingest_staging pipeline;
// import_draft needs Focus-run-continuation semantics). Building a partial
// `ingest_document` now would let other code (or a future session) assume
// the full documented contract works before it does -- the exact
// naming-collision failure mode items.id=383's own text calls out for
// decisions.id=486 vs decisions.id=623's unrelated `source` column. This
// module's commands are named for exactly what they do instead, leaving
// `ingest_document` free for a later item to implement to its full contract
// (possibly by composing store_ingested_document underneath it).
//
// TEXT MIRRORING, NOT EXTRACTION: content is mirrored into the outputs.
// content column (for FTS5 search) only for a small allowlist of trivially
// UTF-8-decodable formats (.txt/.md/.markdown/.html/.htm). PDF/.docx text
// extraction is deferred -- no parsing crate exists in Cargo.toml today, and
// decisions.id=491's own "Text extracted only" R1 format list is a bigger
// lift than this item's storage-model scope. Any file, of any format, is
// still stored losslessly via ingest_blob regardless of whether its content
// could be mirrored -- decisions.id=486's own rationale for retaining the
// document at all ("the document is the audit trail... users may also need
// to retrieve the original") applies independently of whether QR can read
// its text yet.
//
// user_id/persona_id via IPC: same Release-1-no-auth-layer-yet convention
// commands/library.rs's own module header documents. key_hex/master_key are
// derived server-side from KeyRegistry, never passed per-call from the
// frontend.

use std::path::Path;

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::ingest_blob;
use crate::persistence::output_store;

/// Extensions this build will mirror into outputs.content as plain text for
/// FTS5 search. Anything else is still stored via ingest_blob -- just not
/// text-searchable yet. See module header.
const TEXT_MIRROR_EXTENSIONS: &[&str] = &["txt", "md", "markdown", "html", "htm"];

fn is_text_mirror_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|ext| TEXT_MIRROR_EXTENSIONS.contains(&ext.as_str()))
}

#[derive(Debug, Serialize, Type)]
pub struct StoreIngestedDocumentResponse {
    pub output_id: String,
}

/// Store an uploaded/ingested document: writes its bytes to an encrypted
/// blob on disk (ingest_blob), creates a lightweight ingest-only focus_run,
/// and writes the outputs.db row (source='external_ingested').
///
/// Exactly one of `content` (direct paste) or `file_path` (from an OS file
/// picker -- the frontend resolves the path, this command just reads it) is
/// required, mirroring decisions.id=491's own validation rule for the
/// future `ingest_document`.
///
/// `focus_slug`: the real Focus the user filed this document under (may be
/// any string the frontend supplies -- focus_id/focus_slug carry no FK in
/// this codebase, see output_store::INGEST_PSEUDO_FOCUS_ID's own doc
/// comment). `sensitivity`: required, no default -- see
/// output_store::save_ingested_output's VALID_SENSITIVITY check; this build
/// does no automatic content classification (that is decisions.id=488's
/// extraction pass, out of scope here), so silently defaulting to "general"
/// for a potentially sensitive upload would be the wrong failure direction.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn store_ingested_document(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    focus_slug: String,
    project_entity_id: Option<String>,
    sensitivity: String,
    content: Option<String>,
    file_path: Option<String>,
) -> Result<StoreIngestedDocumentResponse, String> {
    if content.is_some() == file_path.is_some() {
        return Err("exactly one of content or file_path is required".to_owned());
    }

    let (master_key, key_hex_str) = key_registry
        .with_key(|k| (k.master_key, key_hex(&k.master_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let (bytes, mirrored_content, original_filename): (Vec<u8>, Option<String>, String) =
        match (content, file_path) {
            (Some(text), None) => {
                let bytes = text.clone().into_bytes();
                (bytes, Some(text), "pasted-content.txt".to_owned())
            }
            (None, Some(path_str)) => {
                let path = Path::new(&path_str);
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|e| format!("could not read file at '{path_str}': {e}"))?;
                let original_filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path_str.clone());
                let mirrored = if is_text_mirror_extension(path) {
                    // Explicit error would be the wrong call here -- an
                    // extension in the mirror allowlist that fails to
                    // decode as UTF-8 is still a legitimate file to store,
                    // it just doesn't get a searchable text mirror.
                    String::from_utf8(bytes.clone()).ok()
                } else {
                    None
                };
                (bytes, mirrored, original_filename)
            }
            _ => unreachable!("validated exactly one of content/file_path above"),
        };

    let output_id = uuid::Uuid::new_v4().to_string();
    let storage_path = ingest_blob::ingest_blob_path(&user_id, &persona_id, &output_id, 1);

    ingest_blob::write_encrypted_blob(&storage_path, &master_key, &persona_id, &bytes)
        .await
        .map_err(|e| e.to_string())?;

    let focus_run_id = output_store::create_ingest_focus_run(&user_id, &persona_id, &key_hex_str)
        .await
        .map_err(|e| e.to_string())?;

    output_store::save_ingested_output(
        &user_id,
        &persona_id,
        &key_hex_str,
        &output_id,
        &focus_run_id,
        "ingested_document",
        &sensitivity,
        &focus_slug,
        project_entity_id.as_deref(),
        &storage_path.to_string_lossy(),
        &original_filename,
        mirrored_content.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(StoreIngestedDocumentResponse { output_id })
}

/// Retrieve an ingested document's current-version bytes, decrypted.
/// Applies the same Focus-profile visibility check as
/// commands::library::get_output (see visibility_focus_id there) -- a
/// Protected-Focus ingested document must be exactly as unreachable as a
/// Protected-Focus QR-generated output.
///
/// No chunking/streaming -- returns the whole file as a JSON byte array (no
/// base64 crate is in Cargo.toml today). Acceptable for typical document
/// sizes; a known, accepted efficiency tradeoff for larger files, not fixed
/// here.
#[tauri::command]
#[specta::specta]
pub async fn get_ingested_document_bytes(
    output_id: String,
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<u8>, String> {
    let (master_key, key_hex_str) = key_registry
        .with_key(|k| (k.master_key, key_hex(&k.master_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let record = output_store::get_output(&user_id, &persona_id, &key_hex_str, &output_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    if super::library::is_protected(
        &pool,
        &persona_id,
        super::library::visibility_focus_id(&record),
    )
    .await?
    {
        return Err("not_found".to_string());
    }

    let storage_path = record
        .storage_path
        .ok_or_else(|| "this document has no stored original file".to_string())?;

    ingest_blob::read_encrypted_blob(Path::new(&storage_path), &master_key, &persona_id)
        .await
        .map_err(|e| e.to_string())
}

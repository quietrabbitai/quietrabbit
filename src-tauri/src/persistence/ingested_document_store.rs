// src-tauri/src/persistence/ingested_document_store.rs
//
// items.id=387 (S1): the three writes of an ingest, moved out of
// commands/ingest.rs so a non-command caller (the hand-off writer,
// conductor::store_writer, S2) can store a document without calling a Tauri
// command. The body is the code items.id=383 and items.id=664 built, unchanged
// but for `output_type`, which the caller now names (the command still passes
// INGESTED_DOCUMENT_OUTPUT_TYPE, so its behaviour is the same).
//
// NO LEASE here. The command takes a writer lease before calling this; a caller
// inside a run already holds the run's lease (auth::writer_gate), so it must not
// take another.

use crate::persistence::ingest_blob;
use crate::persistence::output_store;

/// The output_type of a document the user imported through the Library. The one
/// value commands::ingest has always written.
pub const INGESTED_DOCUMENT_OUTPUT_TYPE: &str = "ingested_document";

/// The three writes of an import: the encrypted blob, the ingest-only
/// focus_run row, the outputs row. If any of them fails the output's blob
/// directory is removed (items.id=664) and the ORIGINAL error is returned, so
/// a failed save leaves nothing on disk. Safe because `output_id` is new in
/// this import and no row references it yet; the caller holds the writer lease
/// across the writes and this cleanup, so a key rotation cannot interleave.
/// (A focus_run row written before a failing output insert stays, as before.)
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub(crate) async fn persist_ingested_document(
    user_id: &str,
    persona_id: &str,
    master_key: &[u8; crate::auth::kdf::MASTER_KEY_LEN],
    key_hex_str: &str,
    output_id: &str,
    output_type: &str,
    focus_slug: &str,
    project_entity_id: Option<&str>,
    sensitivity: &str,
    original_filename: &str,
    bytes: &[u8],
    mirrored_content: Option<&str>,
) -> Result<(), String> {
    // A new document's first blob; matches the storage_version=1 that
    // save_ingested_output inserts below.
    let version = 1;
    let storage_path = ingest_blob::ingest_blob_path(user_id, persona_id, output_id, version);

    let saved = async {
        ingest_blob::write_encrypted_blob(
            &storage_path,
            master_key,
            user_id,
            persona_id,
            output_id,
            version,
            bytes,
        )
        .await
        .map_err(|e| e.to_string())?;

        let focus_run_id = output_store::create_ingest_focus_run(user_id, persona_id, key_hex_str)
            .await
            .map_err(|e| e.to_string())?;

        output_store::save_ingested_output(
            user_id,
            persona_id,
            key_hex_str,
            output_id,
            &focus_run_id,
            output_type,
            sensitivity,
            focus_slug,
            project_entity_id,
            &storage_path.to_string_lossy(),
            original_filename,
            mirrored_content,
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;

    if saved.is_err() {
        // write_encrypted_blob can fail after its rename (the directory
        // fsync), so the cleanup runs for a failed blob write too.
        if let Err(e) = ingest_blob::remove_output_blobs(user_id, persona_id, output_id).await {
            log::warn!(
                "store_ingested_document: removing the blob after a failed save failed: {e}"
            );
        }
    }
    saved
}

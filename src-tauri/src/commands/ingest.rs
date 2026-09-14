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
// TEXT MIRRORING vs EXTRACTION: content is mirrored into the outputs.
// content column (for FTS5 search) two ways. A small allowlist of trivially
// UTF-8-decodable formats (.txt/.md/.markdown/.html/.htm) is mirrored
// verbatim -- see is_text_mirror_extension(). PDF and .docx (items.id=386)
// go through real parsing instead -- see extract_document_text() -- via
// pdf-extract and docx-rust (both pure Rust, Cargo.toml). Either path can
// fail (corrupt file, encrypted/password-protected, a scanned/image-only
// PDF with no text layer, or a panic inside the parsing crate on malformed
// input -- both crates parse untrusted input and pdf-extract's own tracker
// documents open panic reports on exactly that, see extract_document_text's
// doc comment) and always degrades to content: None for that file, never a
// failed upload. Every other format (images, anything not in either path)
// is still stored losslessly via ingest_blob regardless of whether its
// content could be mirrored/extracted -- decisions.id=486's own rationale
// for retaining the document at all ("the document is the audit trail...
// users may also need to retrieve the original") applies independently of
// whether QR can read its text.
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

/// Best-effort text extraction for PDF/.docx, run synchronously -- caller is
/// expected to run this via spawn_blocking (CPU-bound parsing). Returns None
/// on any failure: wrong/missing extension, corrupt or encrypted file, a
/// scanned/image-only PDF with no text layer, or a panic inside the parsing
/// crate itself. The catch_unwind is load-bearing, not defensive-programming
/// filler: both pdf-extract and docx-rust parse arbitrary user-uploaded
/// bytes, and pdf-extract's own tracker documents open panics on exactly
/// that (jrmuizel/pdf-extract#141 "Security hardening: ~50 panic/crash
/// fixes for untrusted PDF input", plus #147/#134/#133/#132/#129 -- all
/// confirmed open this session). Without catching it, a single bad upload
/// would take down the whole async task instead of just losing its search
/// mirror.
fn extract_document_text(path: &Path, bytes: &[u8]) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());

    let result = match ext.as_deref() {
        Some("pdf") => std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes).ok()),
        Some("docx") => std::panic::catch_unwind(|| {
            docx_rust::DocxFile::from_reader(std::io::Cursor::new(bytes))
                .ok()?
                .parse()
                .ok()
                .map(|docx| docx.document.body.text())
        }),
        _ => return None,
    };

    match result {
        Ok(text) => text,
        Err(_) => {
            log::warn!(
                "extract_document_text: parser panicked on '{}', storing without a text mirror",
                path.display()
            );
            None
        }
    }
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
                    let path_owned = path.to_path_buf();
                    let bytes_for_extract = bytes.clone();
                    tokio::task::spawn_blocking(move || {
                        extract_document_text(&path_owned, &bytes_for_extract)
                    })
                    .await
                    .unwrap_or(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal but structurally valid single-page PDF containing
    /// `text` as its only content-stream text, with byte-exact xref offsets
    /// computed from the actual bytes written (not hardcoded) so the
    /// fixture stays correct regardless of `text`'s length.
    fn minimal_pdf_bytes(text: &str) -> Vec<u8> {
        let content = format!("BT /F1 24 Tf 10 100 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> \
             /MediaBox [0 0 200 200] /Contents 5 0 R >>"
                .to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];

        let mut out = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");

        let mut offsets = Vec::with_capacity(objects.len());
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{obj}\nendobj\n", i + 1).as_bytes());
        }

        let xref_offset = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f\r\n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n\r\n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn minimal_docx_bytes(text: &str) -> Vec<u8> {
        let mut docx = docx_rust::Docx::default();
        docx.document
            .push(docx_rust::document::Paragraph::default().push_text(text));
        let cursor = docx
            .write(std::io::Cursor::new(Vec::new()))
            .expect("writing a freshly-built in-memory Docx must not fail");
        cursor.into_inner()
    }

    #[test]
    fn extracts_text_from_a_valid_pdf() {
        let bytes = minimal_pdf_bytes("Hello World");
        let text = extract_document_text(Path::new("upload.pdf"), &bytes);
        assert!(
            text.as_deref().is_some_and(|t| t.contains("Hello")),
            "expected extracted text to contain 'Hello', got {text:?}"
        );
    }

    #[test]
    fn extracts_text_from_a_valid_docx() {
        let bytes = minimal_docx_bytes("Hello World");
        let text = extract_document_text(Path::new("upload.docx"), &bytes);
        assert_eq!(text.as_deref(), Some("Hello World"));
    }

    #[test]
    fn garbage_pdf_bytes_degrade_to_none_without_panicking() {
        let garbage = b"this is not a pdf, just some bytes".to_vec();
        assert_eq!(
            extract_document_text(Path::new("upload.pdf"), &garbage),
            None
        );
    }

    #[test]
    fn garbage_docx_bytes_degrade_to_none_without_panicking() {
        let garbage = b"this is not a docx, just some bytes".to_vec();
        assert_eq!(
            extract_document_text(Path::new("upload.docx"), &garbage),
            None
        );
    }

    #[test]
    fn unrelated_extensions_are_not_extracted() {
        let bytes = b"whatever bytes an image would have".to_vec();
        assert_eq!(extract_document_text(Path::new("upload.png"), &bytes), None);
    }
}

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
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use specta::Type;
use tauri::{Manager, State};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::ingest_blob;
use crate::persistence::output_store;
use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{GenerateOptions, GenerateRequest};

/// Largest document, in bytes, that ingest accepts (items.id=664): applies to a
/// picked file and to pasted content alike. 100 MiB; Chat-PM's choice, change
/// it here only (the user-facing strings below say "100 MB"). Peak memory at
/// the cap is about 3x this for a text file (the shared bytes, plus the
/// ciphertext and assembled file bytes of ingest_blob's seal step), plus
/// whatever the PDF/DOCX parsers expand it to, which nothing here bounds
/// (items.id=665).
const MAX_INGEST_BYTES: u64 = 100 * 1024 * 1024;

/// How long the command waits for PDF/DOCX text extraction before it stores the
/// document without a text mirror (items.id=664). Under writer_gate's
/// COMMAND_DRAIN_CEILING (30 s) because the ingest lease is held across the
/// wait. This stops the WAIT, not the blocking thread: a hung parser keeps its
/// thread and its Arc of the bytes until it returns. Real isolation is
/// items.id=665.
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(20);

// User-facing text: LibraryPane renders the command's error string verbatim, so
// none of these carries a path, an id or an OS error (those go to the log).
const ERR_NOT_REGULAR_FILE: &str = "This isn't a regular file, so it can't be imported.";
const ERR_FILE_TOO_LARGE: &str = "This file is larger than the 100 MB import limit.";
const ERR_PASTE_TOO_LARGE: &str = "The pasted text is larger than the 100 MB import limit.";
const ERR_FILE_UNREADABLE: &str = "That file could not be read.";

fn read_failed(e: std::io::Error) -> String {
    log::warn!("store_ingested_document: reading the file failed: {e}");
    ERR_FILE_UNREADABLE.to_owned()
}

/// Rejects anything that is not a regular file or is over `cap`. Run on the
/// path's metadata first (cheap, and a stat never blocks on a FIFO) and again
/// on the opened handle's, so a path swapped for a FIFO or a larger file in
/// between is still caught.
fn check_regular_and_size(meta: &std::fs::Metadata, cap: u64) -> Result<(), String> {
    if !meta.is_file() {
        return Err(ERR_NOT_REGULAR_FILE.to_owned());
    }
    if meta.len() > cap {
        return Err(ERR_FILE_TOO_LARGE.to_owned());
    }
    Ok(())
}

/// The cap for pasted content, by its UTF-8 byte length. (The IPC layer has
/// already deserialized the whole string by now; this bounds the copies, the
/// blob and the mirror that follow.)
fn check_paste_size(len: usize, cap: u64) -> Result<(), String> {
    if len as u64 > cap {
        return Err(ERR_PASTE_TOO_LARGE.to_owned());
    }
    Ok(())
}

/// Reads at most `cap` bytes from `reader`. `take(cap + 1)` bounds the read
/// even when the source grows past the size it reported; one byte over `cap`
/// is the rejection.
async fn read_capped<R: AsyncRead + Unpin>(
    reader: R,
    size_hint: u64,
    cap: u64,
) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(size_hint.min(cap).saturating_add(1) as usize);
    reader
        .take(cap + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(read_failed)?;
    if buf.len() as u64 > cap {
        return Err(ERR_FILE_TOO_LARGE.to_owned());
    }
    Ok(buf)
}

/// Reads a picked file for ingest: regular files only, at most `cap` bytes.
/// `tokio::fs::metadata` follows symlinks, so a symlink to a FIFO or device is
/// rejected like the FIFO itself. On unix the open is O_NONBLOCK, so a path
/// that became a FIFO after the stat does not block the open; the opened
/// handle is then checked again.
async fn read_ingest_file(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let meta = tokio::fs::metadata(path).await.map_err(read_failed)?;
    check_regular_and_size(&meta, cap)?;
    let (file, len) = open_regular_file(path, cap).await?;
    read_capped(file, len, cap).await
}

/// Opens `path` and checks the OPENED handle (see read_ingest_file). Returns
/// the file and its size.
async fn open_regular_file(path: &Path, cap: u64) -> Result<(tokio::fs::File, u64), String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).await.map_err(read_failed)?;
    let meta = file.metadata().await.map_err(read_failed)?;
    check_regular_and_size(&meta, cap)?;
    Ok((file, meta.len()))
}

/// Waits for a blocking task for at most `limit`. None on timeout or when the
/// task failed. A timeout drops the handle, which detaches the task: it keeps
/// running (see EXTRACTION_TIMEOUT).
async fn await_bounded<T>(handle: tokio::task::JoinHandle<T>, limit: Duration) -> Option<T> {
    match tokio::time::timeout(limit, handle).await {
        Ok(joined) => joined.ok(),
        Err(_) => {
            log::warn!("store_ingested_document: text extraction timed out after {limit:?}");
            None
        }
    }
}

/// Where an ingested document's searchable text mirror comes from. The UTF-8
/// case borrows the shared bytes instead of copying them.
enum Mirror {
    Absent,
    Utf8,
    Extracted(String),
}

impl Mirror {
    fn view<'a>(&'a self, bytes: &'a [u8]) -> Option<&'a str> {
        match self {
            Mirror::Absent => None,
            Mirror::Utf8 => std::str::from_utf8(bytes).ok(),
            Mirror::Extracted(text) => Some(text),
        }
    }
}

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

/// Image extensions eligible for OCR-at-ingestion (items.id=150 Part 2).
/// Deliberately separate from TEXT_MIRROR_EXTENSIONS/extract_document_text:
/// OCR is an async network call to Ollama, run detached in the background
/// after store_ingested_document has already returned -- it does not fit
/// extract_document_text's sync/spawn_blocking/awaited-inline contract,
/// which exists for CPU-bound in-process parsing (see that fn's own doc
/// comment). See the module header comment above this dispatch's plan for
/// the full reasoning.
/// `tiff`/`tif` deliberately excluded (items.id=150 Part 5): confirmed live
/// against Ollama that both formats hard-fail with a 400
/// ("Failed to load image or audio file") regardless of which vision model
/// is targeted -- this is an Ollama-level image-decoding gap, not something
/// either OCR candidate model can work around. png/jpg/jpeg/webp/bmp all
/// decode correctly.
const OCR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];

fn is_ocr_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|ext| OCR_EXTENSIONS.contains(&ext.as_str()))
}

/// Vision-OCR model tag -- single point of change. items.id=150 Part 3's
/// go/no-go eval is decided: PaddleOCR-VL wins, Qwen2.5-VL is rejected
/// (reproducibly emits an immediate stop token / empty output on real
/// document images -- root-caused, not a mystery). See
/// providers::ocr_eval.rs for the eval harness that produced this call.
const OCR_MODEL: &str = "seriouswebby/paddleocr-vl-1.6:latest";

/// PaddleOCR-VL is not instruction-following -- it selects its output mode
/// from a literal short task-prefix ("OCR:", "Table Recognition:",
/// "Spotting:", etc.), per the model's official usage docs. "OCR:" is its
/// plain-document-parsing mode, the correct one here. This must change in
/// lockstep with OCR_MODEL -- a prose instruction like Qwen's old prompt is
/// the wrong convention for this model and silently degrades output.
const OCR_PROMPT: &str = "OCR:";

/// Shared GenerateOptions for OCR generation -- single source of truth for
/// production (`ocr_image_text`, below) and the eval harness
/// (`providers::ocr_eval`, items.id=150 Part 3). OCR_MODEL/OCR_PROMPT are
/// still duplicated as literal copies in ocr_eval.rs (not `pub` here, not
/// worth exporting solely for that eval -- see its own doc comments), but
/// these four fields were never meant to have two independently-editable
/// copies at all, so mirroring that same duplication for them had no design
/// purpose -- it just let num_ctx drift silently out of sync (items.id=150
/// Part 4):
/// eval was raised to 8192 in Part 3, production stayed at the old,
/// too-small 4096 until this fix. num_ctx=8192: Ollama's num_ctx budgets
/// prompt + generation, not prompt-only, and real full-page 300 DPI
/// documents push Qwen2.5-VL:7b's image tokens to its architectural ceiling
/// of exactly 4096 (confirmed via `ollama show` + live load_hparams/
/// task.n_tokens, see ocr_eval.rs's `ocr_options()`/commit f2fd3d9) --
/// 8192 covers that worst-case prompt (~4156) + num_predict (2048) with
/// real headroom. `pub(crate)` only, to let ocr_eval.rs reuse it.
pub(crate) fn ocr_generate_options() -> GenerateOptions {
    GenerateOptions {
        temperature: 0.1,
        top_p: 0.90,
        num_ctx: 8192,
        num_predict: 2048,
    }
}

/// Best-effort OCR for a single image, run outside extract_document_text's
/// sync contract (see OCR_EXTENSIONS's doc comment). None on any failure --
/// Ollama unavailable, timeout, bad model, or an empty response -- same
/// "never fail the upload, just lose the mirror" philosophy as
/// extract_document_text. Mirrors conductor/extract.rs's extract_candidates:
/// a plain async fn against a named model constant, calling
/// OllamaClient::generate() directly, outside the Focus-run/StepExecutor
/// pipeline.
async fn ocr_image_text(client: &OllamaClient, bytes: &[u8]) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let request = GenerateRequest {
        provider_id: None,
        model_id: OCR_MODEL.to_owned(),
        prompt: OCR_PROMPT.to_owned(),
        images: Some(vec![encoded]),
        task_type: "ocr".to_owned(),
        stream: Some(false),
        options: Some(ocr_generate_options()),
    };
    match client.generate(&request).await {
        Ok(resp) if !resp.content.trim().is_empty() => Some(resp.content),
        Ok(_) => None,
        Err(e) => {
            log::warn!("ocr_image_text: model call failed (non-fatal): {e}");
            None
        }
    }
}

/// Push event payload for "ingested-document-content-ready" (items.id=150
/// Part 2). Fires once the detached OCR task finishes, success or failure --
/// same bare "stop waiting, safe to re-fetch" signal as messages.rs's
/// MessageContentReadyPayload/"message-content-ready".
#[derive(Debug, Clone, Serialize, Type)]
pub struct IngestedDocumentContentReadyPayload {
    pub output_id: String,
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

/// The three writes of an import: the encrypted blob, the ingest-only
/// focus_run row, the outputs row. If any of them fails the output's blob
/// directory is removed (items.id=664) and the ORIGINAL error is returned, so
/// a failed save leaves nothing on disk. Safe because `output_id` is new in
/// this import and no row references it yet; the caller holds the writer lease
/// across the writes and this cleanup, so a key rotation cannot interleave.
/// (A focus_run row written before a failing output insert stays, as before.)
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
async fn persist_ingested_document(
    user_id: &str,
    persona_id: &str,
    master_key: &[u8; crate::auth::kdf::MASTER_KEY_LEN],
    key_hex_str: &str,
    output_id: &str,
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
            "ingested_document",
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
    app_handle: tauri::AppHandle,
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

    // items.id=778: an import is three writes (blob, run row, output row); the
    // lease keeps a key rotation from landing between them.
    let _lease = crate::auth::writer_gate::lease_for_session(
        &key_registry,
        crate::auth::writer_gate::Kind::Ingest,
    )
    .await?;
    let (master_key, key_hex_str) = key_registry
        .with_key(|k| (k.data_key, key_hex(&k.data_key)))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // items.id=664: one copy of the document's bytes, shared by the text
    // mirror, the extraction and the background OCR task (Arc, not clones).
    let (bytes, mirror, original_filename, ocr_eligible): (Arc<Vec<u8>>, Mirror, String, bool) =
        match (content, file_path) {
            (Some(text), None) => {
                check_paste_size(text.len(), MAX_INGEST_BYTES)?;
                (
                    Arc::new(text.into_bytes()),
                    Mirror::Utf8,
                    "pasted-content.txt".to_owned(),
                    false,
                )
            }
            (None, Some(path_str)) => {
                let path = Path::new(&path_str);
                let bytes = Arc::new(read_ingest_file(path, MAX_INGEST_BYTES).await?);
                let original_filename = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path_str.clone());
                let ocr_eligible = is_ocr_extension(path);
                let mirror = if is_text_mirror_extension(path) {
                    // Explicit error would be the wrong call here -- an
                    // extension in the mirror allowlist that fails to
                    // decode as UTF-8 is still a legitimate file to store,
                    // it just doesn't get a searchable text mirror.
                    Mirror::Utf8
                } else {
                    let path_owned = path.to_path_buf();
                    let bytes_for_extract = Arc::clone(&bytes);
                    let extraction = tokio::task::spawn_blocking(move || {
                        extract_document_text(&path_owned, &bytes_for_extract)
                    });
                    // A timeout degrades to no text mirror, like a parse
                    // failure: the document is still stored.
                    match await_bounded(extraction, EXTRACTION_TIMEOUT)
                        .await
                        .flatten()
                    {
                        Some(text) => Mirror::Extracted(text),
                        None => Mirror::Absent,
                    }
                };
                (bytes, mirror, original_filename, ocr_eligible)
            }
            _ => unreachable!("validated exactly one of content/file_path above"),
        };

    let output_id = uuid::Uuid::new_v4().to_string();
    persist_ingested_document(
        &user_id,
        &persona_id,
        &master_key,
        &key_hex_str,
        &output_id,
        &focus_slug,
        project_entity_id.as_deref(),
        &sensitivity,
        &original_filename,
        &bytes,
        mirror.view(&bytes),
    )
    .await?;

    // items.id=150 Part 2: OCR runs detached in the background -- it must
    // never delay this command's IPC response (see OCR_EXTENSIONS's doc
    // comment). Failure just leaves content NULL, logged, no retry.
    if ocr_eligible {
        let bg_user_id = user_id.clone();
        let bg_persona_id = persona_id.clone();
        let bg_key_hex = key_hex_str.clone();
        let bg_output_id = output_id.clone();
        let bg_bytes = Arc::clone(&bytes);
        // items.id=778: the OCR backfill writes after this command returns, so
        // it holds a lease of its own. If a rotation is under way the document
        // is already stored; only the searchable text is skipped, and said so.
        let bg_lease = match crate::auth::writer_gate::acquire(
            &user_id,
            crate::auth::writer_gate::Kind::Ingest,
        ) {
            Ok(lease) => Some(lease),
            Err(e) => {
                // The document is stored, but its image text will not be read:
                // tell the user (items.id=778) rather than skip silently.
                crate::write_failure::report(
                    Some(&app_handle),
                    crate::write_failure::Lost::OcrText,
                    &format!("store_ingested_document: OCR text for {output_id} skipped, {e}"),
                );
                None
            }
        };
        tokio::spawn(async move {
            let Some(_bg_lease) = bg_lease else {
                return;
            };
            let client = app_handle.state::<OllamaClient>();
            if let Some(text) = ocr_image_text(&client, &bg_bytes).await {
                // The searchable text read from the image is user content: a
                // failed save is retried and then reported (items.id=778), not
                // logged and dropped. The document itself is already stored.
                let saved = crate::write_failure::retry_transient(|| async {
                    output_store::backfill_ingested_content(
                        &bg_user_id,
                        &bg_persona_id,
                        &bg_key_hex,
                        &bg_output_id,
                        &text,
                    )
                    .await
                    .map_err(|e| e.to_string())
                })
                .await;
                if let Err(e) = saved {
                    crate::write_failure::report(
                        Some(&app_handle),
                        crate::write_failure::Lost::OcrText,
                        &format!(
                            "store_ingested_document: OCR backfill failed for {bg_output_id}: {e}"
                        ),
                    );
                }
            }

            use tauri::Emitter;
            let payload = IngestedDocumentContentReadyPayload {
                output_id: bg_output_id.clone(),
            };
            if let Err(e) = app_handle.emit("ingested-document-content-ready", &payload) {
                log::warn!(
                    "store_ingested_document: emit ingested-document-content-ready failed: {e}"
                );
            }
        });
    }

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
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (master_key, key_hex_str) = key_registry
        .with_key(|k| (k.data_key, key_hex(&k.data_key)))
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

    ingest_blob::read_encrypted_blob(
        Path::new(&storage_path),
        &master_key,
        &user_id,
        &persona_id,
        &output_id,
        record.storage_version,
    )
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

    // Built outside docx-rust (Python zipfile, Word-style part layout), so
    // this exercises the parse path on a file docx-rust's own writer did not
    // produce -- the round-trip test above cannot catch a reader/writer pair
    // that agree with each other but not with real files.
    #[test]
    fn extracts_text_from_a_word_style_docx_fixture() {
        let bytes = include_bytes!("../../tests/fixtures/word_style.docx");
        let text = extract_document_text(Path::new("upload.docx"), bytes);
        assert_eq!(
            text.as_deref(),
            Some("Quiet Rabbit\r\nSplit across two runs.")
        );
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

    #[test]
    fn recognizes_ocr_eligible_extensions() {
        assert!(is_ocr_extension(Path::new("scan.png")));
        assert!(is_ocr_extension(Path::new("photo.JPG")));
        assert!(!is_ocr_extension(Path::new("doc.pdf")));
        assert!(!is_ocr_extension(Path::new("notes.txt")));
    }

    /// Regression guard (items.id=150 Part 5): tiff/tif were removed from
    /// OCR_EXTENSIONS after confirming live that Ollama hard-fails decoding
    /// both formats regardless of model. Must not silently come back.
    #[test]
    fn rejects_tiff_extensions() {
        assert!(!is_ocr_extension(Path::new("scan.tiff")));
        assert!(!is_ocr_extension(Path::new("scan.tif")));
    }

    // -- bounded reads (items.id=664) ----------------------------------------

    #[test]
    fn the_cap_is_100_mib_and_the_strings_say_so() {
        assert_eq!(MAX_INGEST_BYTES, 100 * 1024 * 1024);
        assert!(ERR_FILE_TOO_LARGE.contains("100 MB"));
        assert!(ERR_PASTE_TOO_LARGE.contains("100 MB"));
    }

    #[tokio::test]
    async fn a_file_at_the_cap_is_read_and_one_byte_over_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let at_cap = dir.path().join("at_cap.bin");
        let over = dir.path().join("over.bin");
        std::fs::write(&at_cap, vec![7u8; 64]).unwrap();
        std::fs::write(&over, vec![7u8; 65]).unwrap();

        assert_eq!(read_ingest_file(&at_cap, 64).await.unwrap(), vec![7u8; 64]);
        assert_eq!(
            read_ingest_file(&over, 64).await.unwrap_err(),
            ERR_FILE_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn an_empty_file_is_read_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        assert!(read_ingest_file(&empty, 64).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_file_over_the_real_cap_is_rejected_without_being_read() {
        // Sparse: no disk or memory is used, and the metadata check rejects it
        // before any byte is read.
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_INGEST_BYTES + 1)
            .unwrap();
        assert_eq!(
            read_ingest_file(&big, MAX_INGEST_BYTES).await.unwrap_err(),
            ERR_FILE_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn a_source_that_outgrows_its_reported_size_is_still_bounded() {
        // An endless reader stands in for a file that grows during the read.
        let err = read_capped(tokio::io::repeat(0u8), 0, 1024)
            .await
            .unwrap_err();
        assert_eq!(err, ERR_FILE_TOO_LARGE);

        let exact = read_capped(&b"abcd"[..], 4, 4).await.unwrap();
        assert_eq!(exact, b"abcd");
    }

    #[tokio::test]
    async fn a_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_ingest_file(dir.path(), 64).await.unwrap_err(),
            ERR_NOT_REGULAR_FILE
        );
    }

    #[tokio::test]
    async fn a_missing_file_gives_the_generic_message_without_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("secret-name.pdf");
        let err = read_ingest_file(&missing, 64).await.unwrap_err();
        assert_eq!(err, ERR_FILE_UNREADABLE);
        assert!(!err.contains("secret-name"));
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_rejected_and_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        make_fifo(&fifo);

        let result = tokio::time::timeout(Duration::from_secs(5), read_ingest_file(&fifo, 64))
            .await
            .expect("a FIFO must be rejected, not waited on");
        assert_eq!(result.unwrap_err(), ERR_NOT_REGULAR_FILE);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_to_a_fifo_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let link = dir.path().join("link");
        make_fifo(&fifo);
        std::os::unix::fs::symlink(&fifo, &link).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), read_ingest_file(&link, 64))
            .await
            .expect("a symlink to a FIFO must be rejected, not waited on");
        assert_eq!(result.unwrap_err(), ERR_NOT_REGULAR_FILE);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_fifo_directly_is_non_blocking_and_checked_on_the_handle() {
        // The stat in read_ingest_file normally rejects a FIFO first. This is
        // the path swapped for a FIFO between that stat and the open: the open
        // must not block (O_NONBLOCK) and the handle check must reject it.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        make_fifo(&fifo);

        let result = tokio::time::timeout(Duration::from_secs(5), open_regular_file(&fifo, 64))
            .await
            .expect("opening a FIFO must not block");
        assert_eq!(result.err().unwrap(), ERR_NOT_REGULAR_FILE);
    }

    #[test]
    fn pasted_content_is_capped_by_utf8_length() {
        assert!(check_paste_size(64, 64).is_ok());
        assert_eq!(check_paste_size(65, 64).unwrap_err(), ERR_PASTE_TOO_LARGE);
        // 'é' is two bytes: 33 characters are 66 bytes.
        let multibyte = "é".repeat(33);
        assert_eq!(multibyte.chars().count(), 33);
        assert_eq!(
            check_paste_size(multibyte.len(), 64).unwrap_err(),
            ERR_PASTE_TOO_LARGE
        );
    }

    // -- bounded wait on extraction (items.id=664) ---------------------------

    #[tokio::test]
    async fn a_hung_blocking_task_stops_being_waited_on() {
        let started = std::time::Instant::now();
        let handle = tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_secs(2));
            Some("late".to_owned())
        });
        let got = await_bounded(handle, Duration::from_millis(50)).await;
        assert_eq!(got, None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the wait must end at the limit, not when the task does"
        );
    }

    #[tokio::test]
    async fn a_task_that_finishes_in_time_returns_its_value() {
        let handle = tokio::task::spawn_blocking(|| 41 + 1);
        assert_eq!(
            await_bounded(handle, Duration::from_secs(5)).await,
            Some(42)
        );
    }

    #[tokio::test]
    async fn a_panicking_blocking_task_is_none_not_a_panic() {
        let handle = tokio::task::spawn_blocking(|| -> u8 { panic!("parser blew up") });
        assert_eq!(await_bounded(handle, Duration::from_secs(5)).await, None);
    }

    #[test]
    fn the_mirror_borrows_utf8_bytes_and_drops_invalid_ones() {
        let valid = "héllo".as_bytes();
        assert_eq!(Mirror::Utf8.view(valid), Some("héllo"));
        assert_eq!(Mirror::Utf8.view(&[0xff, 0xfe]), None);
        assert_eq!(Mirror::Absent.view(valid), None);
        assert_eq!(
            Mirror::Extracted("text".to_owned()).view(&[0xff]),
            Some("text")
        );
    }

    // -- no blob survives a failed save (items.id=664) -----------------------

    const TEST_USER: &str = "ingest-664-user";
    const TEST_PERSONA: &str = "ingest-664-persona";
    const TEST_DATA_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0x5au8; crate::auth::kdf::MASTER_KEY_LEN];

    /// Points QR_DATA_ROOT at a fresh tempdir for one test and restores it.
    struct DataRoot {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl DataRoot {
        async fn new() -> Self {
            let lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().expect("failed to create tempdir");
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

    fn blob_dir(output_id: &str) -> std::path::PathBuf {
        ingest_blob::ingest_blob_path(TEST_USER, TEST_PERSONA, output_id, 1)
            .parent()
            .unwrap()
            .to_path_buf()
    }

    async fn persist(output_id: &str, key_hex_str: &str, sensitivity: &str) -> Result<(), String> {
        persist_ingested_document(
            TEST_USER,
            TEST_PERSONA,
            &TEST_DATA_KEY,
            key_hex_str,
            output_id,
            "writing-assistant",
            None,
            sensitivity,
            "doc.txt",
            b"the document",
            Some("the document"),
        )
        .await
    }

    #[tokio::test]
    async fn a_successful_save_keeps_the_blob_and_the_row() {
        let _root = DataRoot::new().await;
        let key = key_hex(&TEST_DATA_KEY);

        persist("out-ok", &key, "general").await.expect("must save");

        let path = ingest_blob::ingest_blob_path(TEST_USER, TEST_PERSONA, "out-ok", 1);
        let plain = ingest_blob::read_encrypted_blob(
            &path,
            &TEST_DATA_KEY,
            TEST_USER,
            TEST_PERSONA,
            "out-ok",
            1,
        )
        .await
        .expect("the blob must be readable");
        assert_eq!(plain, b"the document");
        let record = output_store::get_output(TEST_USER, TEST_PERSONA, &key, "out-ok")
            .await
            .unwrap();
        assert!(record.is_some(), "the outputs row must exist");
    }

    #[tokio::test]
    async fn a_failed_output_insert_leaves_no_blob() {
        let _root = DataRoot::new().await;
        let key = key_hex(&TEST_DATA_KEY);

        // The sensitivity check fails save_ingested_output after the blob and
        // the focus_run row have been written.
        let err = persist("out-bad-sensitivity", &key, "not-a-sensitivity")
            .await
            .unwrap_err();

        assert!(err.contains("Invalid sensitivity"), "original error: {err}");
        assert!(
            !blob_dir("out-bad-sensitivity").exists(),
            "the blob directory must be removed"
        );
    }

    #[tokio::test]
    async fn a_failed_run_row_insert_leaves_no_blob() {
        let _root = DataRoot::new().await;
        let key = key_hex(&TEST_DATA_KEY);
        // outputs.db now exists under `key`; opening it under another key
        // fails create_ingest_focus_run after the blob was written.
        persist("out-first", &key, "general").await.unwrap();
        let wrong_key = key_hex(&[0x6bu8; crate::auth::kdf::MASTER_KEY_LEN]);

        let result = persist("out-wrong-key", &wrong_key, "general").await;

        assert!(result.is_err(), "a wrong database key must fail the save");
        assert!(
            !blob_dir("out-wrong-key").exists(),
            "the blob directory must be removed"
        );
        assert!(
            blob_dir("out-first").exists(),
            "an earlier, saved document's blob must be untouched"
        );
    }
}

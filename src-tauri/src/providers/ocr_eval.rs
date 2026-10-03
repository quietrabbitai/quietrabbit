//! OCR accuracy check against real document images — originally a two-model
//! go/no-go eval (items.id=150 Part 3), simplified to the single production
//! model once that eval was decided (Part 5: PaddleOCR-VL won, Qwen2.5-VL
//! was removed rather than kept as a dead comparison target — see git
//! history/commit messages for the rejected candidate's constants and the
//! reasoning for dropping them instead of relabeling them).
//!
//! Compares the production vision-OCR model against real document images
//! with known ground-truth transcriptions. This is NOT the routing/hardware
//! calibration harness (`providers::evaluation`) — that harness scores
//! latency + structural format compliance for text-only models, which has
//! no concept of "how close is this transcription to the correct text."
//!
//! Whole module is `#[cfg(test)]`-gated at the `mod` declaration in
//! `providers/mod.rs` — compiles to nothing in production builds. The eval
//! itself is additionally `#[ignore]`d so plain `cargo test` (the CI gate)
//! never touches Ollama or reads `QR_OCR_EVAL_DIR`.
//!
//! Sample images and their ground-truth transcriptions are real personal
//! documents (paperless-ngx corpus) and must NEVER be git-tracked — this
//! repo pushes to a public GitHub remote. They live entirely outside the
//! repo tree, at a path supplied via `QR_OCR_EVAL_DIR`:
//!
//! ```text
//! $QR_OCR_EVAL_DIR/
//!   manifest.json   -- [{"image": "doc001.png", "ground_truth": "..."}]
//!   doc001.png
//!   doc002.jpg
//!   ...
//! ```
//!
//! Run manually (never as part of plain `cargo test`):
//! ```text
//! export QR_OCR_EVAL_DIR=/path/outside/the/repo
//! cargo test ocr_accuracy_eval -- --ignored --nocapture
//! ```
//!
//! Reports Character Error Rate (Levenshtein distance over a
//! whitespace-normalized, case/punctuation-preserving comparison) and
//! latency per document, plus an aggregate summary. Deliberately has no
//! pass/fail threshold — this is measurement for a human go/no-go call, not
//! a verdict. In `ocr_accuracy_eval`, only filenames/doc IDs and numbers are
//! ever printed — never the transcribed text or ground truth themselves.
//!
//! `ocr_dump_transcriptions` is a second, independent mode for when typing
//! up ground truth isn't practical (e.g. a batch of large, text-heavy real
//! documents). It needs no `manifest.json` -- it reads every supported file
//! directly from `QR_OCR_EVAL_DIR` and prints the model's raw transcription
//! so a human can eyeball quality directly. This is the one deliberate
//! exception to the no-content-printed rule above: its entire purpose is
//! showing the transcribed text.
//! ```text
//! cargo test ocr_dump_transcriptions -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde::Deserialize;

use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{GenerateOptions, GenerateRequest};

/// Vision-OCR model wired into production ingestion
/// (`commands::ingest::OCR_MODEL`). Duplicated here rather than imported:
/// `ingest.rs`'s constant isn't `pub`, and exporting it solely to serve this
/// eval isn't worth touching shipped code for. Must be kept in sync by hand
/// if that tag ever changes. Confirmed via `ollama show --modelfile` on the
/// actual pulled model: the official PaddleOCR-VL GGUF release,
/// byte-identical tensors (sha256-verified), correctly-converted Ollama
/// prompt template, vision projector included.
///
/// items.id=150 Part 3 evaluated this against Qwen2.5-VL and rejected Qwen
/// (reproducible immediate-stop-token/empty-output failure on real document
/// images); Part 5 removed Qwen's constants from this file entirely rather
/// than keeping them as a labeled dead comparison target -- see git history
/// if that baseline is ever needed again.
const PADDLEOCR_VL_MODEL: &str = "seriouswebby/paddleocr-vl-1.6:latest";

/// Literal copy of `commands::ingest::OCR_PROMPT` — kept identical so this
/// eval matches what production actually sends. PaddleOCR-VL is NOT
/// instruction-following -- it selects its output mode from a literal short
/// task-prefix ("OCR:", "Table Recognition:", "Spotting:", etc.), per the
/// model's official usage docs and the GGUF card's llama.cpp example
/// (`-p 'OCR:'`). "OCR:" is the plain-document-parsing mode, the correct one
/// here.
const PADDLEOCR_VL_PROMPT: &str = "OCR:";

fn ocr_options() -> GenerateOptions {
    // Single source of truth is now commands::ingest::ocr_generate_options()
    // (items.id=150 Part 4) -- see its doc comment for the num_ctx=8192
    // derivation (both candidates' architecturally-capped image-token cost,
    // measured live against Ollama). This wrapper exists only so call sites
    // here keep reading "ocr eval's options" rather than reaching into
    // commands::ingest twice.
    crate::commands::ingest::ocr_generate_options()
}

// ---------------------------------------------------------------------------
// Manifest loading
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    image: String,
    ground_truth: String,
}

fn eval_dir() -> PathBuf {
    let raw = std::env::var("QR_OCR_EVAL_DIR").unwrap_or_else(|_| {
        panic!(
            "QR_OCR_EVAL_DIR not set -- point it at a directory (outside this \
             repo) containing manifest.json + sample images. See the doc \
             comment at the top of providers/ocr_eval.rs."
        )
    });
    PathBuf::from(raw)
}

fn load_manifest() -> Vec<ManifestEntry> {
    let dir = eval_dir();
    let manifest_path = dir.join("manifest.json");

    let text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "failed to read manifest at {}: {e}",
            manifest_path.display()
        )
    });

    let entries: Vec<ManifestEntry> = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "malformed manifest.json at {}: {e}",
            manifest_path.display()
        )
    });

    assert!(
        !entries.is_empty(),
        "manifest.json at {} is empty -- need at least one sample",
        manifest_path.display()
    );

    for entry in &entries {
        assert!(
            !entry.image.trim().is_empty() && !entry.ground_truth.trim().is_empty(),
            "manifest entry has empty image or ground_truth: {entry:?}"
        );
        let img_path = dir.join(&entry.image);
        assert!(
            img_path.is_file(),
            "manifest references {} but it does not exist",
            img_path.display()
        );
    }

    entries
}

// ---------------------------------------------------------------------------
// Image loading (PDF-aware)
// ---------------------------------------------------------------------------

/// Jason's real sample set is scanned-image PDFs (no embedded text layer,
/// just a scanned page inside a PDF wrapper) -- `pdf-extract`
/// (`commands::ingest::extract_document_text`) is useless here since there's
/// nothing to extract. Every sample in this batch is single-page, so only
/// page 1 is ever rendered -- no multi-page handling is built.
fn is_pdf(image_name: &str) -> bool {
    Path::new(image_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// OCR-appropriate render resolution for scanned pages.
const PDF_RENDER_DPI: &str = "300";

/// Render a PDF's first page to PNG bytes by shelling out to `pdftoppm`
/// (poppler-utils). Chosen over pdfium-render/mupdf/poppler-rs bindings --
/// see the plan for the full comparison -- because it needs zero new Cargo
/// dependency (poppler-utils is already installed, a near-universal Linux
/// desktop dependency), avoids mupdf's AGPL licensing question entirely, and
/// avoids pdfium-render's manual prebuilt-binary download step.
fn render_pdf_first_page_to_png(pdf_path: &Path) -> Vec<u8> {
    let scratch = tempfile::tempdir().expect("failed to create scratch tempdir for PDF render");
    let prefix = scratch.path().join("page"); // pdftoppm appends ".png" itself

    let output = Command::new("pdftoppm")
        .args([
            "-f",
            "1",
            "-l",
            "1",
            "-singlefile",
            "-r",
            PDF_RENDER_DPI,
            "-png",
        ])
        .arg(pdf_path)
        .arg(&prefix)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to run pdftoppm (is poppler-utils installed? \
                 `pacman -S poppler` / `apt install poppler-utils`): {e}"
            )
        });

    assert!(
        output.status.success(),
        "pdftoppm failed on {}: {}",
        pdf_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let rendered = prefix.with_extension("png");
    std::fs::read(&rendered).unwrap_or_else(|e| {
        panic!(
            "pdftoppm reported success but {} is missing: {e}",
            rendered.display()
        )
    })
    // `scratch` (TempDir) drops at end of scope -- cleans itself up.
}

/// Load an image's bytes, rendering page 1 first if it's a PDF. Runs once
/// per document (not once per candidate model) -- both models are compared
/// against identical bytes either way.
fn load_image_bytes(dir: &Path, image_name: &str) -> Vec<u8> {
    let path = dir.join(image_name);
    if is_pdf(image_name) {
        render_pdf_first_page_to_png(&path)
    } else {
        std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Directory listing (dump mode -- no manifest.json needed)
// ---------------------------------------------------------------------------

/// Mirrors `commands::ingest::OCR_EXTENSIONS` plus `pdf` (which production
/// ingestion doesn't handle -- see the PDF-handling section above).
/// Duplicated locally for the same reason `OCR_MODEL` is: not `pub` there.
const SAMPLE_EXTENSIONS: &[&str] = &["pdf", "png", "jpg", "jpeg", "webp", "bmp"];

fn is_supported_sample(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            SAMPLE_EXTENSIONS
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(e))
        })
        .unwrap_or(false)
}

/// Lists every supported sample file directly in `dir` -- no manifest, no
/// ground truth. Used by `ocr_dump_transcriptions` only; `ocr_accuracy_eval`
/// still goes through `load_manifest()`. Extension filtering means a stray
/// `manifest.json` left over from accuracy-eval use in the same directory is
/// automatically ignored, no special-casing needed.
fn list_sample_files(dir: &Path) -> Vec<String> {
    let read_dir = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read QR_OCR_EVAL_DIR at {}: {e}", dir.display()));

    let mut names: Vec<String> = read_dir
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_supported_sample(name))
        .collect();

    names.sort(); // deterministic output order
    assert!(
        !names.is_empty(),
        "no .pdf/image files found in {} -- point QR_OCR_EVAL_DIR at a directory of samples",
        dir.display()
    );
    names
}

// ---------------------------------------------------------------------------
// Character Error Rate
// ---------------------------------------------------------------------------

/// Trim + collapse whitespace runs (including newlines) to a single space.
/// Case and punctuation are preserved deliberately -- line-wrap placement is
/// a formatting artifact, but case (names, acronyms) and punctuation (dates,
/// amounts) carry real meaning in these documents and must stay significant
/// to the metric.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Character Error Rate: Levenshtein distance over ground-truth length.
/// `None` when the normalized ground truth is empty (division undefined) --
/// shouldn't happen given manifest validation, but guarded rather than
/// panicking mid-eval.
fn compute_cer(output: &str, ground_truth: &str) -> Option<f64> {
    let normalized_output = normalize(output);
    let normalized_truth = normalize(ground_truth);
    let truth_len = normalized_truth.chars().count();
    if truth_len == 0 {
        return None;
    }
    let distance = strsim::levenshtein(&normalized_output, &normalized_truth);
    Some(distance as f64 / truth_len as f64)
}

// ---------------------------------------------------------------------------
// Result rows + report
// ---------------------------------------------------------------------------

enum Row {
    Ok {
        doc: String,
        cer: f64,
        latency_ms: f64,
    },
    NoGroundTruth {
        doc: String,
    },
    Failed {
        doc: String,
        reason: String,
    },
}

fn print_report(rows: &[Row]) {
    println!("\n=== Per-document results ({PADDLEOCR_VL_MODEL}) ===");
    println!("{:<20} {:>10} {:>12}", "doc_id", "cer", "latency_ms");
    for row in rows {
        match row {
            Row::Ok {
                doc,
                cer,
                latency_ms,
            } => println!("{doc:<20} {cer:>10.4} {latency_ms:>12.1}"),
            Row::NoGroundTruth { doc } => {
                println!("{doc:<20} {:>10} {:>12}", "N/A", "-")
            }
            Row::Failed { doc, reason } => {
                println!("{doc:<20} FAILED (reason: {reason})")
            }
        }
    }

    println!("\n=== Aggregate summary ===");
    println!(
        "{:>6} {:>10} {:>10} {:>12} {:>10} {:>16}",
        "n_ok", "n_failed", "mean_cer", "median_cer", "max_cer", "mean_latency_ms"
    );
    let oks: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Ok {
                cer, latency_ms, ..
            } => Some((*cer, *latency_ms)),
            _ => None,
        })
        .collect();
    let n_failed = rows
        .iter()
        .filter(|r| matches!(r, Row::Failed { .. }))
        .count();

    if oks.is_empty() {
        println!(
            "{:>6} {:>10} {:>10} {:>12} {:>10} {:>16}",
            0, n_failed, "-", "-", "-", "-"
        );
        return;
    }

    let mut cers: Vec<f64> = oks.iter().map(|(c, _)| *c).collect();
    cers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean_cer = cers.iter().sum::<f64>() / cers.len() as f64;
    let median_cer = cers[cers.len() / 2];
    let max_cer = *cers.last().unwrap();
    let mean_latency = oks.iter().map(|(_, l)| *l).sum::<f64>() / oks.len() as f64;

    println!(
        "{:>6} {:>10} {mean_cer:>10.4} {median_cer:>12.4} {max_cer:>10.4} {mean_latency:>16.1}",
        oks.len(),
        n_failed,
    );
}

// ---------------------------------------------------------------------------
// Eval
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn ocr_accuracy_eval() {
    // items.id=586: this test runs standalone, with no Tauri app bootstrap,
    // so `ollama_sidecar::ensure_available()` never runs in this process
    // and the client's trust gate would otherwise stay fail-closed forever.
    // Test-only escape hatch -- never callable from production code.
    crate::ollama_sidecar::force_trust_for_test(true);

    let entries = load_manifest();
    let dir = eval_dir();
    let client = OllamaClient::new();

    // Health check up front -- never send doomed requests to a model that
    // isn't actually pulled.
    let health = client.check_health().await;
    assert!(
        health
            .available_models
            .iter()
            .any(|a| a == PADDLEOCR_VL_MODEL),
        "{PADDLEOCR_VL_MODEL} not found in Ollama available_models -- \
         `ollama pull {PADDLEOCR_VL_MODEL}` first"
    );

    let mut rows = Vec::new();
    for entry in &entries {
        let bytes = load_image_bytes(&dir, &entry.image);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let request = GenerateRequest {
            provider_id: None,
            model_id: PADDLEOCR_VL_MODEL.to_owned(),
            prompt: PADDLEOCR_VL_PROMPT.to_owned(),
            images: Some(vec![encoded]),
            task_type: "ocr".to_owned(),
            stream: Some(false),
            options: Some(ocr_options()),
        };

        match client.generate(&request).await {
            Ok(resp) => match compute_cer(&resp.content, &entry.ground_truth) {
                Some(cer) => rows.push(Row::Ok {
                    doc: entry.image.clone(),
                    cer,
                    latency_ms: resp.latency_ms,
                }),
                None => rows.push(Row::NoGroundTruth {
                    doc: entry.image.clone(),
                }),
            },
            Err(e) => rows.push(Row::Failed {
                doc: entry.image.clone(),
                reason: e.to_string(),
            }),
        }
    }

    print_report(&rows);
}

// ---------------------------------------------------------------------------
// Dump mode -- no ground truth, no CER, just raw transcriptions
// ---------------------------------------------------------------------------

/// For when typing up ground truth isn't practical (e.g. a batch of large,
/// text-heavy real documents). Runs the production model against every
/// supported file in `QR_OCR_EVAL_DIR` directly (no `manifest.json`) and
/// prints its raw transcription so a human can eyeball quality directly --
/// dates, dollar amounts, names -- instead of a numeric score against
/// hand-typed ground truth. See the module doc comment for the printed-
/// content exception this makes.
#[tokio::test]
#[ignore]
async fn ocr_dump_transcriptions() {
    // items.id=586: see ocr_accuracy_eval's identical comment above.
    crate::ollama_sidecar::force_trust_for_test(true);

    let dir = eval_dir();
    let files = list_sample_files(&dir);
    let client = OllamaClient::new();

    let health = client.check_health().await;
    assert!(
        health
            .available_models
            .iter()
            .any(|a| a == PADDLEOCR_VL_MODEL),
        "{PADDLEOCR_VL_MODEL} not found in Ollama available_models -- \
         `ollama pull {PADDLEOCR_VL_MODEL}` first"
    );

    for name in &files {
        println!("\n{}", "=".repeat(70));
        println!("Document: {name}");
        println!("{}", "=".repeat(70));

        let bytes = load_image_bytes(&dir, name);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let request = GenerateRequest {
            provider_id: None,
            model_id: PADDLEOCR_VL_MODEL.to_owned(),
            prompt: PADDLEOCR_VL_PROMPT.to_owned(),
            images: Some(vec![encoded]),
            task_type: "ocr".to_owned(),
            stream: Some(false),
            options: Some(ocr_options()),
        };

        match client.generate(&request).await {
            Ok(resp) => {
                println!("(latency: {:.1}ms)", resp.latency_ms);
                println!("{}", resp.content);
            }
            Err(e) => println!("FAILED: {e}"), // non-fatal, same as ocr_accuracy_eval
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the metric itself -- these run under plain `cargo test`
// (no Ollama, no env var), so the CI gate exercises the CER logic even
// though the live eval above stays `#[ignore]`d.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod metric_tests {
    use super::*;

    #[test]
    fn cer_identical_strings_is_zero() {
        assert_eq!(compute_cer("hello world", "hello world"), Some(0.0));
    }

    #[test]
    fn cer_whitespace_only_difference_is_zero() {
        // Line-wrap/newline differences are formatting artifacts, not errors.
        assert_eq!(compute_cer("hello\nworld", "hello world"), Some(0.0));
    }

    #[test]
    fn cer_case_difference_is_counted() {
        // Case is preserved deliberately -- must not normalize it away.
        let cer = compute_cer("Hello World", "hello world").unwrap();
        assert!(cer > 0.0);
    }

    #[test]
    fn cer_punctuation_difference_is_counted() {
        let cer = compute_cer("03/14/2026", "03-14-2026").unwrap();
        assert!(cer > 0.0);
    }

    #[test]
    fn cer_empty_ground_truth_is_none() {
        assert_eq!(compute_cer("anything", ""), None);
    }

    #[test]
    fn cer_completely_wrong_output_is_bounded_by_length() {
        let cer = compute_cer("xxxxx", "abc").unwrap();
        assert!(cer >= 1.0);
    }
}

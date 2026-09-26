//! One-time OCR accuracy go/no-go eval — items.id=150 Part 3.
//!
//! Compares candidate Ollama vision models against real document images
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
//! latency per model, per document, plus an aggregate summary. Deliberately
//! has no pass/fail threshold — this is measurement for a human go/no-go
//! call, not a verdict. In `ocr_accuracy_eval`, only filenames/doc IDs and
//! numbers are ever printed — never the transcribed text or ground truth
//! themselves.
//!
//! `ocr_dump_transcriptions` is a second, independent mode for when typing
//! up ground truth isn't practical (e.g. a batch of large, text-heavy real
//! documents). It needs no `manifest.json` -- it reads every supported file
//! directly from `QR_OCR_EVAL_DIR` and prints each candidate's raw
//! transcription so a human can eyeball quality directly. This is the one
//! deliberate exception to the no-content-printed rule above: its entire
//! purpose is showing the transcribed text.
//! ```text
//! cargo test ocr_dump_transcriptions -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde::Deserialize;

use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{GenerateOptions, GenerateRequest};

/// A candidate model plus the prompt convention it expects. Kept together
/// at the point of definition (rather than a `match model_id { .. }` lookup
/// elsewhere) so a new candidate can't be added without also supplying its
/// prompt -- a match would need a fallback arm that could silently paper
/// over a missing case.
struct Candidate {
    model_id: &'static str,
    prompt: &'static str,
}

/// Vision-OCR model already wired into production ingestion
/// (`commands::ingest::OCR_MODEL`). Duplicated here rather than imported:
/// `ingest.rs`'s constant isn't `pub`, and exporting it solely to serve this
/// one-time eval isn't worth touching Part 2's shipped code for. Must be
/// kept in sync by hand if that tag ever changes.
const QWEN_MODEL: &str = "qwen2.5vl:7b";

/// Confirmed via `ollama show --modelfile` on the actual pulled model: the
/// official PaddleOCR-VL GGUF release, byte-identical tensors
/// (sha256-verified), correctly-converted Ollama prompt template, vision
/// projector included.
const PADDLEOCR_VL_MODEL: &str = "seriouswebby/paddleocr-vl-1.6:latest";

/// Literal copy of `commands::ingest::OCR_PROMPT` — kept identical so the
/// comparison matches what production actually sends. Qwen2.5-VL is a
/// general instruction-following vision model, so this prose instruction is
/// a fair prompt for it.
const QWEN_OCR_PROMPT: &str = "Transcribe all text visible in this image, verbatim, \
    preserving reading order and line breaks. Output only the transcribed \
    text -- no commentary, no markdown fences, no description of the image.";

/// PaddleOCR-VL is NOT instruction-following -- it selects its output mode
/// from a literal short task-prefix ("OCR:", "Table Recognition:",
/// "Spotting:", etc.), per the model's official usage docs and the GGUF
/// card's llama.cpp example (`-p 'OCR:'`). "OCR:" is the plain-document
/// -parsing mode, the correct one for this comparison -- sending it the
/// Qwen-style prose instruction would test the wrong prompt convention and
/// invalidate the comparison.
const PADDLEOCR_VL_PROMPT: &str = "OCR:";

const CANDIDATES: [Candidate; 2] = [
    Candidate {
        model_id: QWEN_MODEL,
        prompt: QWEN_OCR_PROMPT,
    },
    Candidate {
        model_id: PADDLEOCR_VL_MODEL,
        prompt: PADDLEOCR_VL_PROMPT,
    },
];

fn ocr_options() -> GenerateOptions {
    // NOT the same as production ocr_image_text() anymore -- that still uses
    // num_ctx: 4096, a known, separate defect (flagged, not fixed here; this
    // dispatch is eval-only). 4096 silently caps out on real full-page
    // documents: confirmed live against Ollama (journalctl -u ollama) that a
    // real page's prompt hit 4130-4148 tokens and got a hard 400 ("request
    // (4130 tokens) exceeds the available context size (4096 tokens)").
    //
    // num_ctx is prompt + generation, not prompt-only, so the right target
    // is worst-case prompt + num_predict. Both candidates' image-token cost
    // is architecturally capped by their own mtmd projector's
    // image_max_pixels, not something that grows open-endedly with DPI or
    // document density -- confirmed via ollama's load_hparams log lines and
    // matching task.n_tokens on a synthetic full-page 300 DPI image:
    //   Qwen2.5-VL:7b      image_max_pixels 3211264 -> hard cap of exactly
    //                      4096 image tokens (3211264 / 28px-merged-patch^2).
    //                      Worst case ~= 4096 + ~60 (prompt text) + 2048
    //                      (num_predict) ~= 6204.
    //   PaddleOCR-VL       image_max_pixels 1605632 -> ~2048 image tokens,
    //                      roughly half Qwen's -- its smaller vision encoder
    //                      does not scale per-pixel the same way Qwen's
    //                      does. Worst case ~= 2048 + ~5 ("OCR:") + 2048
    //                      ~= 4101.
    // 8192 is the next clean power-of-two above the higher (Qwen) worst
    // case, giving both real headroom without being wastefully large next
    // to either model's actual context_length (128000/131072). Shared
    // rather than per-candidate (contrast `prompt`, which had to split):
    // both worst cases fit comfortably under one generous value, and the
    // memory cost of the smaller PaddleOCR-VL model running at 8192 instead
    // of a tighter number is negligible.
    GenerateOptions {
        temperature: 0.1,
        top_p: 0.90,
        num_ctx: 8192,
        num_predict: 2048,
    }
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
const SAMPLE_EXTENSIONS: &[&str] = &["pdf", "png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif"];

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
        model: &'static str,
        cer: f64,
        latency_ms: f64,
    },
    NoGroundTruth {
        doc: String,
        model: &'static str,
    },
    Failed {
        doc: String,
        model: &'static str,
        reason: String,
    },
}

fn print_report(rows: &[Row]) {
    println!("\n=== Per-document results ===");
    println!(
        "{:<20} {:<20} {:>10} {:>12}",
        "model_id", "doc_id", "cer", "latency_ms"
    );
    for row in rows {
        match row {
            Row::Ok {
                doc,
                model,
                cer,
                latency_ms,
            } => println!("{model:<20} {doc:<20} {cer:>10.4} {latency_ms:>12.1}"),
            Row::NoGroundTruth { doc, model } => {
                println!("{model:<20} {doc:<20} {:>10} {:>12}", "N/A", "-")
            }
            Row::Failed { doc, model, reason } => {
                println!("{model:<20} {doc:<20} FAILED (reason: {reason})")
            }
        }
    }

    println!("\n=== Aggregate summary ===");
    println!(
        "{:<20} {:>6} {:>10} {:>10} {:>12} {:>10} {:>16}",
        "model_id", "n_ok", "n_failed", "mean_cer", "median_cer", "max_cer", "mean_latency_ms"
    );
    for candidate in &CANDIDATES {
        let model = candidate.model_id;
        let oks: Vec<(f64, f64)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Ok {
                    model: m,
                    cer,
                    latency_ms,
                    ..
                } if *m == model => Some((*cer, *latency_ms)),
                _ => None,
            })
            .collect();
        let n_failed = rows
            .iter()
            .filter(|r| matches!(r, Row::Failed { model: m, .. } if *m == model))
            .count();

        if oks.is_empty() {
            println!(
                "{model:<20} {:>6} {:>10} {:>10} {:>12} {:>10} {:>16}",
                0, n_failed, "-", "-", "-", "-"
            );
            continue;
        }

        let mut cers: Vec<f64> = oks.iter().map(|(c, _)| *c).collect();
        cers.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean_cer = cers.iter().sum::<f64>() / cers.len() as f64;
        let median_cer = cers[cers.len() / 2];
        let max_cer = *cers.last().unwrap();
        let mean_latency = oks.iter().map(|(_, l)| *l).sum::<f64>() / oks.len() as f64;

        println!(
            "{model:<20} {:>6} {:>10} {mean_cer:>10.4} {median_cer:>12.4} {max_cer:>10.4} {mean_latency:>16.1}",
            oks.len(),
            n_failed,
        );
    }
}

// ---------------------------------------------------------------------------
// Eval
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn ocr_accuracy_eval() {
    let entries = load_manifest();
    let dir = eval_dir();
    let client = OllamaClient::new();

    // Health check up front -- never send doomed requests to a model that
    // isn't actually pulled.
    let health = client.check_health().await;
    let runnable: Vec<&Candidate> = CANDIDATES
        .iter()
        .filter(|c| {
            let ok = health.available_models.iter().any(|a| a == c.model_id);
            if !ok {
                let m = c.model_id;
                println!(
                    "[SKIP] {m} not found in Ollama available_models -- `ollama pull {m}` first"
                );
            }
            ok
        })
        .collect();
    assert!(
        !runnable.is_empty(),
        "no candidate models available -- nothing to evaluate"
    );

    // Paired per-document run: every runnable model against the same image.
    let mut rows = Vec::new();
    for entry in &entries {
        let bytes = load_image_bytes(&dir, &entry.image);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        for &candidate in &runnable {
            let request = GenerateRequest {
                provider_id: None,
                model_id: candidate.model_id.to_owned(),
                prompt: candidate.prompt.to_owned(),
                images: Some(vec![encoded.clone()]),
                task_type: "ocr".to_owned(),
                stream: Some(false),
                options: Some(ocr_options()),
            };

            match client.generate(&request).await {
                Ok(resp) => match compute_cer(&resp.content, &entry.ground_truth) {
                    Some(cer) => rows.push(Row::Ok {
                        doc: entry.image.clone(),
                        model: candidate.model_id,
                        cer,
                        latency_ms: resp.latency_ms,
                    }),
                    None => rows.push(Row::NoGroundTruth {
                        doc: entry.image.clone(),
                        model: candidate.model_id,
                    }),
                },
                Err(e) => rows.push(Row::Failed {
                    doc: entry.image.clone(),
                    model: candidate.model_id,
                    reason: e.to_string(),
                }),
            }
        }
    }

    print_report(&rows);
}

// ---------------------------------------------------------------------------
// Dump mode -- no ground truth, no CER, just raw transcriptions
// ---------------------------------------------------------------------------

/// For when typing up ground truth isn't practical (e.g. a batch of large,
/// text-heavy real documents). Runs both candidates against every supported
/// file in `QR_OCR_EVAL_DIR` directly (no `manifest.json`) and prints each
/// model's raw transcription so a human can eyeball quality directly --
/// dates, dollar amounts, names -- instead of a numeric score against
/// hand-typed ground truth. See the module doc comment for the printed-
/// content exception this makes.
#[tokio::test]
#[ignore]
async fn ocr_dump_transcriptions() {
    let dir = eval_dir();
    let files = list_sample_files(&dir);
    let client = OllamaClient::new();

    let health = client.check_health().await;
    let runnable: Vec<&Candidate> = CANDIDATES
        .iter()
        .filter(|c| {
            let ok = health.available_models.iter().any(|a| a == c.model_id);
            if !ok {
                let m = c.model_id;
                println!(
                    "[SKIP] {m} not found in Ollama available_models -- `ollama pull {m}` first"
                );
            }
            ok
        })
        .collect();
    assert!(
        !runnable.is_empty(),
        "no candidate models available -- nothing to evaluate"
    );

    for name in &files {
        println!("\n{}", "=".repeat(70));
        println!("Document: {name}");
        println!("{}", "=".repeat(70));

        let bytes = load_image_bytes(&dir, name);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        for &candidate in &runnable {
            let request = GenerateRequest {
                provider_id: None,
                model_id: candidate.model_id.to_owned(),
                prompt: candidate.prompt.to_owned(),
                images: Some(vec![encoded.clone()]),
                task_type: "ocr".to_owned(),
                stream: Some(false),
                options: Some(ocr_options()),
            };

            println!("\n--- {} ---", candidate.model_id);
            match client.generate(&request).await {
                Ok(resp) => {
                    println!("(latency: {:.1}ms)", resp.latency_ms);
                    println!("{}", resp.content);
                }
                Err(e) => println!("FAILED: {e}"), // non-fatal, same as ocr_accuracy_eval
            }
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

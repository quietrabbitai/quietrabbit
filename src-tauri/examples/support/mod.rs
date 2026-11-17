// src-tauri/examples/support/mod.rs
//
// DEV-ONLY, shared by the calibration examples (focus_calibration.rs and
// calibration_harness.rs; items.id=899 stage 2). Extracted from focus_calibration.rs
// with no change of behaviour: the Ollama transport, the sidecar start, the
// template render, the file helpers. Each example includes it with
// `#[path = "support/mod.rs"] mod support;`. Not an example itself (cargo only
// builds examples/*.rs and examples/*/main.rs).
//
// Everything here talks only to QR's own sidecar on 127.0.0.1 (or a base URL the
// caller passes). No network beyond that, no hosted call, no key.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quietrabbit_lib::ollama_sidecar::{self, OllamaSidecar, OllamaSource};
use quietrabbit_lib::providers::utils::set_data_root;
use serde_json::{json, Value};

/// The three bundled local models, in run order. The 3b model goes first because
/// the shared analysis step already loaded it.
pub const MODELS: [&str; 3] = ["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"];

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Generated {
    pub text: String,
    /// Reasoning returned in Ollama's separate `thinking` field, if any.
    pub thinking: String,
    pub latency_ms: f64,
    pub load_ms: f64,
    pub output_tokens: u64,
    pub hit_cap: bool,
}

pub async fn generate(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    prompt: &str,
    options: &Value,
) -> Result<Generated, String> {
    let payload = json!({"model": model, "prompt": prompt, "stream": false, "options": options});
    let start = Instant::now();
    let resp = http
        .post(format!("{base_url}/api/generate"))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("bad body: {e}"))?;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    let output_tokens = body["eval_count"].as_u64().unwrap_or(0);
    let cap = options["num_predict"].as_u64().unwrap_or(0);
    Ok(Generated {
        text: body["response"].as_str().unwrap_or_default().to_owned(),
        thinking: body["thinking"].as_str().unwrap_or_default().to_owned(),
        latency_ms,
        load_ms: body["load_duration"].as_f64().unwrap_or(0.0) / 1.0e6,
        output_tokens,
        hit_cap: body["done_reason"].as_str() == Some("length")
            || (cap > 0 && output_tokens >= cap),
    })
}

/// Splits a <think>...</think> block out of the text (some reasoning models put it
/// inline). Returns (visible draft, reasoning if any).
pub fn strip_think(text: &str) -> (String, Option<String>) {
    if let (Some(a), Some(b)) = (text.find("<think>"), text.find("</think>")) {
        if a < b {
            let reasoning = text[a + "<think>".len()..b].trim().to_owned();
            let mut t = text[..a].to_owned();
            t.push_str(&text[b + "</think>".len()..]);
            return (t.trim().to_owned(), Some(reasoning));
        }
    }
    (text.trim().to_owned(), None)
}

pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

pub fn slug(model: &str) -> String {
    model.replace(':', "-")
}

pub fn write(path: &Path, content: &str) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    }
    fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Loads a model with a one-token call so the first real sample does not carry the
/// load time. Not recorded; the time is printed.
/// `options` must carry the same num_ctx (and num_gpu) as the real calls, or Ollama
/// reloads the model for the first real call and the load lands in its latency.
pub async fn warm_up(http: &reqwest::Client, base_url: &str, model: &str, options: &Value) {
    let mut options = options.clone();
    options["num_predict"] = json!(1);
    let start = Instant::now();
    let r = generate(http, base_url, model, "Say ok.", &options).await;
    match r {
        Ok(_) => println!("  warm-up {model}: {:.1}s", start.elapsed().as_secs_f64()),
        Err(e) => println!("  warm-up {model}: failed ({e})"),
    }
}

/// Starts QR's own sidecar against a scratch data root whose ollama_models is a
/// symlink to the real models directory. Exits the process if it does not start.
/// Returns the base URL.
pub async fn start_sidecar(
    sidecar: &mut OllamaSidecar,
    home: &Path,
    root: &Path,
    models_dir: Option<PathBuf>,
) -> String {
    let real_models =
        models_dir.unwrap_or_else(|| home.join(".local/share/ai.quietrabbit.app/ollama_models"));
    assert!(
        real_models.is_dir(),
        "models dir {} not found",
        real_models.display()
    );
    let scratch_root = root.join("dataroot");
    fs::create_dir_all(&scratch_root).expect("scratch data root");
    let link = scratch_root.join("ollama_models");
    if link.symlink_metadata().is_err() {
        std::os::unix::fs::symlink(&real_models, &link).expect("symlink ollama_models");
    }
    set_data_root(scratch_root.clone());
    // An empty resource dir so only the debug PATH fallback is tried, not the
    // bundled stub script (which exits at once and would burn the port list).
    let empty_resources = scratch_root.join("no-resources");
    fs::create_dir_all(&empty_resources).expect("resources dir");
    println!(
        "sidecar: scratch data root {}, models {} (symlink)",
        scratch_root.display(),
        real_models.display()
    );
    let startup = sidecar.ensure_available(&empty_resources).await;
    if startup.source != OllamaSource::Sidecar {
        eprintln!("QR's sidecar did not start; nothing was run");
        std::process::exit(1);
    }
    let base = format!("http://127.0.0.1:{}", ollama_sidecar::port());
    println!("sidecar: ready at {base}");
    base
}

/// Same rule as executor.rs render_template(): replace each {token}, strip any
/// leftover {token}, trim. A leftover here is a bug in this example, so it panics.
pub fn render_template(template: &str, tokens: &BTreeMap<&str, String>) -> String {
    let mut out = template.to_owned();
    for (k, v) in tokens {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    let mut rest = out.as_str();
    while let Some(i) = rest.find('{') {
        if let Some(j) = rest[i..].find('}') {
            let name = &rest[i + 1..i + j];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                panic!("unresolved token {{{name}}} in rendered prompt");
            }
            rest = &rest[i + 1..];
        } else {
            break;
        }
    }
    out.trim().to_owned()
}

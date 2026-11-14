// src-tauri/examples/focus_calibration.rs
//
// DEV-ONLY. Not part of the shipped path: nothing in the app calls this, it is
// never built into a release artifact, and it is run by hand on a developer
// machine. items.id=893 (build B6a of items.id=892, decisions.id=942 condition c).
//
// Purpose: give a human real local-model writing to read BEFORE any quality bar
// is set. Nobody has measured whether the bundled local models can write an
// acceptable document. This example generates five synthetic briefs across the
// three bundled models, two samples each (30 generations), saves every output,
// and writes one side-by-side comparison page per brief.
//
// What it measures (and nothing else): word count, latency, and hits against a
// phrase list. It does not score quality. A human reads the pages.
//
// Rules this file follows:
//   - Synthetic data only. Every persona, employer, city, device and voice
//     profile below is invented. No real persona data is read.
//   - No hosted models. The main run talks only to QR's own sidecar.
//   - persist_result is never called, so scores.db is never created.
//   - Outputs go under $HOME/.cache/qr-892-calibration/<run>/, never /tmp.
//
// Prompts: rendered from the SHIPPED Writing Assistant templates in
// schema/shared_038.sql (read at compile time with include_str!, not copied), so
// the prompt text is whatever a user would get today. Two shipped steps are used:
//   voice_analysis (task_type quick_response -> llama3.2:3b in select_model) fills
//   {voice_analysis_output} for
//   draft_writing (the template this calibration is about).
// The analysis step is run once per brief and sample with llama3.2:3b and shared
// by the three drafting models, so the models differ only in the draft step. The
// quality_check and voice_transform steps are not run: this measures drafting.
//
// Render rule: executor.rs render_template() is private to the crate, so the same
// rule is repeated here (String::replace of {token}, unresolved tokens stripped,
// result trimmed). If that function changes, re-sync this file. Options are the
// shipped step's options_override over the executor defaults (temperature 0.5,
// top_p 0.90, num_predict 1024), with num_ctx from get_context_window()'s table
// (4096 for llama3.2:3b, 8192 for the others).
//
// Transport: GenerateRequest cannot be built outside the crate (pub(crate)
// field), so this example posts the same /api/generate payload OllamaClient builds
// straight to the sidecar port. The sidecar itself is QR's own OllamaSidecar,
// started the way a dev run starts it (see "Sidecar" below). The shipped client
// has a 120 s timeout; this example waits longer (REQUEST_TIMEOUT) so a slow
// generation is still measured, and flags every one that passed the shipped 120 s.
//
// Sidecar: QR starts its own Ollama on 127.0.0.1:21434 (decisions.id=840). The
// models live in QuietRabbit's own ollama_models directory. To avoid writing
// ollama_sidecar.pid and reclaim state into the real app data directory, the
// example sets a scratch data root, <run-root>/dataroot/, whose ollama_models is a
// SYMLINK to the real directory (override: --models-dir). Debug builds only: the
// debug PATH fallback launches the machine's `ollama` binary, because the bundled
// binary is a stub (items.id=583). OLLAMA_NO_CLOUD and the environment allowlist
// come from the shipped sidecar code. Release builds cannot run this example.
//
// The suppress list below is an INTERIM, example-local list of common AI-tell
// phrases. It is NOT the QR floor: language-defaults.guide (decisions.id=441) is
// not built yet, so there is nothing shipped to read. Replace it with the real
// list when it exists. Hits are reported per phrase, never as a score.
//
// Usage (from src-tauri/):
//   cargo run --example focus_calibration                      # main run
//   cargo run --example focus_calibration -- --approve-long-run
//   cargo run --example focus_calibration -- --resume <run>    # reuse finished files
//   cargo run --example focus_calibration -- --system-pass --resume <run>
// The system pass is the optional hand-run experiment on the system Ollama
// (127.0.0.1:11434): same five briefs, one sample, gemma4:12b and deepseek-r1:32b.
// It writes under <run>/system-ollama-experiment/ and is not part of the shipped
// path. Run it only after the main run and only when asked.
// System pass details: a generation (or model load) that runs past 10 minutes stops
// that model for the rest of the pass and the partial results are kept. The output cap
// is 4096 tokens instead of 1024 because reasoning models spend output tokens on their
// reasoning. Reasoning (Ollama's `thinking` field or an inline <think> block) is saved
// beside the draft as <model>-1.reasoning.txt and never appears in the .md or on the
// comparison page. Models are unloaded (keep_alive 0) when the pass ends.
// --system-num-gpu N sets Ollama's num_gpu for the pass (0 = CPU only), for when
// another app holds the GPU memory. The index title then says so.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quietrabbit_lib::ollama_sidecar::{self, OllamaSidecar, OllamaSource};
use quietrabbit_lib::providers::utils::set_data_root;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SHARED_038: &str = include_str!("../schema/shared_038.sql");

/// The three bundled local models, in run order. The 3b model goes first because
/// the shared analysis step already loaded it.
const MODELS: [&str; 3] = ["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"];
const SAMPLES: u32 = 2;

/// The second pass: system Ollama, one sample, hand-run only.
const SYSTEM_PASS_MODELS: [&str; 2] = ["gemma4:12b", "deepseek-r1:32b"];
const SYSTEM_OLLAMA: &str = "http://127.0.0.1:11434";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// System pass: one generation (or model load) past this stops that model for the
/// rest of the pass. The partial results are kept and reported.
const SYSTEM_PASS_LIMIT: Duration = Duration::from_secs(10 * 60);
/// System pass output cap. The shipped step caps at 1024 tokens, but a reasoning
/// model spends output tokens on its reasoning, so 1024 could leave no draft. Every
/// draft in the main run was well under it, so a higher cap does not change length.
const SYSTEM_PASS_NUM_PREDICT: u64 = 4096;
const SHIPPED_CLIENT_TIMEOUT_MS: f64 = 120_000.0;
const LONG_RUN_LIMIT_SECS: f64 = 3600.0;

/// Interim suppress list (see header). Lowercase; matched as substrings on the
/// lowercased text. The em dash is counted too: heavy use is a common tell.
const SUPPRESS_LIST: &[&str] = &[
    "delve",
    "tapestry",
    "in today's fast-paced",
    "it's important to note",
    "it is important to note",
    "i hope this email finds you well",
    "i hope this message finds you well",
    "i hope this letter finds you well",
    "please do not hesitate",
    "don't hesitate to reach out",
    "navigate the complexities",
    "testament to",
    "unlock",
    "leverage",
    "seamless",
    "robust",
    "in conclusion",
    "furthermore",
    "moreover",
    "i am writing to express",
    "game-changer",
    "embark",
    "realm",
    "ever-evolving",
    "rest assured",
    "stands as",
    "\u{2014}",
];

struct Persona {
    job_title: &'static str,
    employer: &'static str,
    location_city: &'static str,
    /// Invented device. The shipped template has no device token, so it appears in
    /// the request text of the briefs where a user would state it.
    #[allow(dead_code)]
    device: &'static str,
}

struct Voice {
    tone: &'static str,
    formality: &'static str,
    directness: &'static str,
    length_preference: &'static str,
    pacing: &'static str,
}

struct Brief {
    id: &'static str,
    title: &'static str,
    user_input: &'static str,
    persona: Persona,
    voice: Voice,
}

fn briefs() -> Vec<Brief> {
    vec![
        Brief {
            id: "rma-letter",
            title: "RMA letter",
            user_input: "Write a letter to the returns department asking for a return \
                authorization for my Vantelle X14 laptop, serial VX14-0042718, bought on \
                3 August 2026 from Brightwater Electronics, order BW-558201. The left hinge \
                cracked in the second week and the screen now wobbles. I want a replacement, \
                not a refund, and I can ship it at my own cost if they send a label.",
            persona: Persona {
                job_title: "Warehouse supervisor",
                employer: "Halvorsen Marine Supply",
                location_city: "Duluth",
                device: "Vantelle X14 laptop",
            },
            voice: Voice {
                tone: "direct",
                formality: "moderate",
                directness: "direct",
                length_preference: "concise",
                pacing: "quick",
            },
        },
        Brief {
            id: "cover-letter",
            title: "Cover letter",
            user_input: "Write a cover letter for a logistics coordinator job at Northgate \
                Freight Partners in Tacoma. I have six years in warehouse operations, I \
                cut mis-picks by a third at my current job by redoing the bin labelling, and \
                I taught myself spreadsheet reporting so the team stopped doing it by hand. \
                I want to move into planning work.",
            persona: Persona {
                job_title: "Warehouse supervisor",
                employer: "Halvorsen Marine Supply",
                location_city: "Duluth",
                device: "Vantelle X14 laptop",
            },
            voice: Voice {
                tone: "warm",
                formality: "moderate",
                directness: "balanced",
                length_preference: "concise",
                pacing: "medium",
            },
        },
        Brief {
            id: "complaint-email",
            title: "Formal complaint email",
            user_input: "Write a formal complaint email to the customer service manager of \
                Corvid Broadband. My TR-220 router has dropped the connection every evening \
                for five weeks. I logged four support tickets (CB-77120, CB-77391, CB-77804, \
                CB-78213) and was told each time to restart it. A technician visit on 14 \
                October never happened and nobody called. I want the fault fixed, a credit \
                for the weeks I paid for service I did not get, and a named contact.",
            persona: Persona {
                job_title: "Accounts payable clerk",
                employer: "Mirelle & Dunn Accounting",
                location_city: "Spokane",
                device: "Corvid TR-220 router",
            },
            voice: Voice {
                tone: "formal",
                formality: "formal",
                directness: "direct",
                length_preference: "detailed",
                pacing: "deliberate",
            },
        },
        Brief {
            id: "personal-note",
            title: "Short personal note",
            user_input: "Write a short note to my neighbour Dorrie thanking her for keeping \
                an eye on the house and watering the tomatoes while we were away for ten \
                days. We left her a jar of the lemon curd she liked. Keep it to a few lines.",
            persona: Persona {
                job_title: "Accounts payable clerk",
                employer: "Mirelle & Dunn Accounting",
                location_city: "Spokane",
                device: "Corvid TR-220 router",
            },
            voice: Voice {
                tone: "warm",
                formality: "casual",
                directness: "balanced",
                length_preference: "concise",
                pacing: "quick",
            },
        },
        Brief {
            id: "essay-section",
            title: "One-page essay section",
            user_input: "Write one section, about a page long, of an essay arguing that small \
                towns should keep their paper-based public notice boards even when the council \
                also posts online. Cover who gets left out by online-only notices, and that a \
                board in the library or post office is read by people who never visit the \
                council website. Plain argument, no list.",
            persona: Persona {
                job_title: "Library assistant",
                employer: "Cedar Hollow Public Library",
                location_city: "Bozeman",
                device: "Pellgrove 11 tablet",
            },
            voice: Voice {
                tone: "balanced",
                formality: "moderate",
                directness: "diplomatic",
                length_preference: "detailed",
                pacing: "deliberate",
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// Shipped template extraction
// ---------------------------------------------------------------------------

struct ShippedStep {
    prompt_template: String,
    options_override: Value,
}

/// Reads the `<composition>:default` customization row out of shared_038.sql. The
/// JSON literal is on the line after the row id; SQL doubles apostrophes.
fn shipped_step(row_id: &str) -> ShippedStep {
    let marker = format!("('{row_id}'");
    let mut lines = SHARED_038.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with(&marker) {
            let literal = lines
                .next()
                .unwrap_or_else(|| panic!("shared_038.sql: no JSON line after {row_id}"))
                .trim();
            let body = literal
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix("',"))
                .unwrap_or_else(|| panic!("shared_038.sql: unexpected literal shape for {row_id}"));
            let v: Value = serde_json::from_str(&body.replace("''", "'"))
                .unwrap_or_else(|e| panic!("shared_038.sql: {row_id} is not JSON: {e}"));
            return ShippedStep {
                prompt_template: v["prompt_template"]
                    .as_str()
                    .expect("prompt_template")
                    .to_owned(),
                options_override: v["options_override"].clone(),
            };
        }
    }
    panic!("shared_038.sql: row {row_id} not found");
}

/// Same rule as executor.rs render_template(): replace each {token}, strip any
/// leftover {token}, trim. A leftover here is a bug in this example, so it panics.
fn render_template(template: &str, tokens: &BTreeMap<&str, String>) -> String {
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

/// Same format as executor.rs format_voice_profile(): the five allowed keys,
/// sorted, `key=value`, joined by ", ".
fn format_voice(v: &Voice) -> String {
    format!(
        "directness={}, formality={}, length_preference={}, pacing={}, tone={}",
        v.directness, v.formality, v.length_preference, v.pacing, v.tone
    )
}

fn options_for(step: &ShippedStep, model: &str) -> Value {
    let mut o = json!({"temperature": 0.5, "top_p": 0.90, "num_predict": 1024});
    if let Some(m) = step.options_override.as_object() {
        for (k, val) in m {
            o[k] = val.clone();
        }
    }
    // executor.rs get_context_window() table for the three bundled models.
    o["num_ctx"] = json!(match model {
        "llama3.2:3b" => 4096,
        "llama3.1:8b" | "qwen2.5:7b" => 8192,
        _ => 2048,
    });
    o
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    brief: String,
    model: String,
    sample: u32,
    base_url: String,
    words: usize,
    latency_ms: f64,
    load_ms: f64,
    output_tokens: u64,
    hit_cap: bool,
    over_shipped_timeout: bool,
    suppress_hits: BTreeMap<String, usize>,
    error: Option<String>,
}

struct Generated {
    text: String,
    /// Reasoning returned in Ollama's separate `thinking` field, if any.
    thinking: String,
    latency_ms: f64,
    load_ms: f64,
    output_tokens: u64,
    hit_cap: bool,
}

async fn generate(
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
fn strip_think(text: &str) -> (String, Option<String>) {
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

fn suppress_hits(text: &str) -> BTreeMap<String, usize> {
    let lower = text.to_lowercase();
    SUPPRESS_LIST
        .iter()
        .filter_map(|p| {
            let n = lower.matches(p).count();
            (n > 0).then(|| ((*p).to_owned(), n))
        })
        .collect()
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn slug(model: &str) -> String {
    model.replace(':', "-")
}

fn write(path: &Path, content: &str) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    }
    fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// One generation, reusing a finished file when resuming.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    http: &reqwest::Client,
    base_url: &str,
    out_dir: &Path,
    brief: &Brief,
    model: &str,
    sample: u32,
    prompt: &str,
    options: &Value,
) -> Record {
    let stem = format!("{}-{sample}", slug(model));
    let md = out_dir.join(brief.id).join(format!("{stem}.md"));
    let meta = out_dir.join(brief.id).join(format!("{stem}.json"));
    if let (true, Ok(m)) = (md.exists(), fs::read_to_string(&meta)) {
        if let Ok(r) = serde_json::from_str::<Record>(&m) {
            println!("  {stem}: reused");
            return r;
        }
    }

    let (text, g, error) = match generate(http, base_url, model, prompt, options).await {
        Ok(g) => {
            let (visible, inline) = strip_think(&g.text);
            // Reasoning is kept apart from the draft, never in the .md.
            let reasoning = [Some(g.thinking.clone()), inline]
                .into_iter()
                .flatten()
                .filter(|r| !r.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            if !reasoning.is_empty() {
                write(
                    &out_dir.join(brief.id).join(format!("{stem}.reasoning.txt")),
                    &reasoning,
                );
            }
            (visible, Some(g), None)
        }
        Err(e) => (String::new(), None, Some(e)),
    };
    let record = Record {
        brief: brief.id.to_owned(),
        model: model.to_owned(),
        sample,
        base_url: base_url.to_owned(),
        words: word_count(&text),
        latency_ms: g.as_ref().map_or(0.0, |g| g.latency_ms),
        load_ms: g.as_ref().map_or(0.0, |g| g.load_ms),
        output_tokens: g.as_ref().map_or(0, |g| g.output_tokens),
        hit_cap: g.as_ref().is_some_and(|g| g.hit_cap),
        over_shipped_timeout: g
            .as_ref()
            .is_some_and(|g| g.latency_ms > SHIPPED_CLIENT_TIMEOUT_MS),
        suppress_hits: suppress_hits(&text),
        error,
    };
    write(&md, &text);
    write(
        &meta,
        &serde_json::to_string_pretty(&record).expect("serialize record"),
    );
    println!(
        "  {stem}: {} words, {:.1}s{}{}{}",
        record.words,
        record.latency_ms / 1000.0,
        if record.hit_cap {
            ", hit token cap"
        } else {
            ""
        },
        if record.over_shipped_timeout {
            ", over shipped 120 s timeout"
        } else {
            ""
        },
        record
            .error
            .as_deref()
            .map_or(String::new(), |e| format!(", ERROR {e}")),
    );
    record
}

/// Loads a model with a one-token call so the first real sample does not carry the
/// load time. Not recorded; the time is printed.
/// `options` must carry the same num_ctx (and num_gpu) as the real calls, or Ollama
/// reloads the model for the first real call and the load lands in its latency.
async fn warm_up(http: &reqwest::Client, base_url: &str, model: &str, options: &Value) {
    let mut options = options.clone();
    options["num_predict"] = json!(1);
    let start = Instant::now();
    let r = generate(http, base_url, model, "Say ok.", &options).await;
    match r {
        Ok(_) => println!("  warm-up {model}: {:.1}s", start.elapsed().as_secs_f64()),
        Err(e) => println!("  warm-up {model}: failed ({e})"),
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const PAGE_CSS: &str = "body{font-family:system-ui,sans-serif;margin:1.5rem;max-width:1500px;\
    color:#1b1b1b;background:#fafafa}h1{font-size:1.3rem}table{border-collapse:collapse}\
    td,th{border:1px solid #ccc;padding:.3rem .6rem;text-align:left;vertical-align:top}\
    .grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:1rem}\
    .cell{background:#fff;border:1px solid #ccc;padding:.8rem}.cell h2{font-size:1rem;margin:0}\
    .meta{color:#555;font-size:.85rem;margin:.2rem 0 .6rem}\
    pre{white-space:pre-wrap;word-wrap:break-word;font-family:Georgia,serif;font-size:.95rem;\
    line-height:1.45;margin:0}.warn{color:#9a4b00}.note{background:#fff7e0;padding:.6rem;\
    border:1px solid #e6d28a;margin:0 0 1rem}\
    @media (prefers-color-scheme:dark){body{background:#161616;color:#e8e8e8}\
    .cell{background:#202020;border-color:#444}td,th{border-color:#444}.meta{color:#aaa}\
    .note{background:#2b2616;border-color:#6b5a1f}a{color:#8ab4f8}}";

fn meta_line(r: &Record) -> String {
    let hits: Vec<String> = r
        .suppress_hits
        .iter()
        .map(|(p, n)| format!("{} x{n}", esc(p.trim())))
        .collect();
    let mut s = format!(
        "{} words &middot; {:.1} s &middot; suppress-list hits: {}",
        r.words,
        r.latency_ms / 1000.0,
        if hits.is_empty() {
            "none".to_owned()
        } else {
            hits.join(", ")
        }
    );
    if r.hit_cap {
        s.push_str(" &middot; <span class=warn>stopped at the 1024-token cap</span>");
    }
    if r.over_shipped_timeout {
        s.push_str(" &middot; <span class=warn>over the shipped 120 s timeout</span>");
    }
    if let Some(e) = &r.error {
        s.push_str(&format!(
            " &middot; <span class=warn>ERROR {}</span>",
            esc(e)
        ));
    }
    s
}

fn compare_page(out_dir: &Path, brief: &Brief, models: &[&str], samples: u32, records: &[Record]) {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{} comparison</title><style>{PAGE_CSS}</style><h1>{}</h1>\
         <p class=note>Synthetic brief. Generated by focus_calibration (dev-only). Metrics are word count, \
         latency and hits on an interim phrase list. Nothing here is scored.</p>\
         <details open><summary>The request</summary><pre>{}</pre></details>\
         <p class=meta>Context given: role {}, organisation {}, city {}. Voice: {}.</p><div class=grid>",
        esc(brief.title),
        esc(brief.title),
        esc(brief.user_input),
        esc(brief.persona.job_title),
        esc(brief.persona.employer),
        esc(brief.persona.location_city),
        esc(&format_voice(&brief.voice)),
    );
    for n in 1..=samples {
        for m in models {
            let r = records
                .iter()
                .find(|r| r.brief == brief.id && r.model == *m && r.sample == n);
            let text =
                fs::read_to_string(out_dir.join(brief.id).join(format!("{}-{n}.md", slug(m))))
                    .unwrap_or_default();
            let reasoning_file = format!("{}-{n}.reasoning.txt", slug(m));
            let reasoning_link = if out_dir.join(brief.id).join(&reasoning_file).exists() {
                format!(
                    " &middot; <a href=\"{reasoning_file}\">reasoning (kept separate, not shown here)</a>"
                )
            } else {
                String::new()
            };
            h.push_str(&format!(
                "<div class=cell><h2>{} &middot; sample {n}</h2><p class=meta>{}{reasoning_link}</p><pre>{}</pre></div>",
                esc(m),
                r.map_or("not run".to_owned(), meta_line),
                esc(&text)
            ));
        }
    }
    h.push_str("</div>");
    write(&out_dir.join(brief.id).join("compare.html"), &h);
}

fn index_page(out_dir: &Path, run: &str, title: &str, bs: &[Brief], records: &[Record]) {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{PAGE_CSS}</style><h1>{title} &middot; {}</h1>\
         <p class=note>Dev-only calibration read (items.id=893). Open a brief to read the drafts side by side. \
         The suppress list is an interim, example-local list, not the QR floor.</p>",
        esc(run)
    );
    for b in bs {
        h.push_str(&format!(
            "<h2><a href=\"{0}/compare.html\">{1}</a></h2><table><tr><th>Model</th><th>Sample</th>\
             <th>Words</th><th>Latency</th><th>Suppress-list hits</th><th>Notes</th></tr>",
            b.id,
            esc(b.title)
        ));
        for r in records.iter().filter(|r| r.brief == b.id) {
            let hits: usize = r.suppress_hits.values().sum();
            let mut notes = Vec::new();
            if r.hit_cap {
                notes.push("token cap".to_owned());
            }
            if r.over_shipped_timeout {
                notes.push("over 120 s".to_owned());
            }
            if let Some(e) = &r.error {
                notes.push(format!("ERROR {}", esc(e)));
            }
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1} s</td><td>{hits}</td><td>{}</td></tr>",
                esc(&r.model),
                r.sample,
                r.words,
                r.latency_ms / 1000.0,
                notes.join(", ")
            ));
        }
        h.push_str("</table>");
    }
    write(&out_dir.join("index.html"), &h);
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

struct Args {
    approve_long_run: bool,
    system_pass: bool,
    resume: Option<String>,
    models_dir: Option<PathBuf>,
    system_num_gpu: Option<u32>,
}

fn parse_args() -> Args {
    let mut a = Args {
        approve_long_run: false,
        system_pass: false,
        resume: None,
        models_dir: None,
        system_num_gpu: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--approve-long-run" => a.approve_long_run = true,
            "--system-pass" => a.system_pass = true,
            "--resume" => a.resume = it.next(),
            "--models-dir" => a.models_dir = it.next().map(PathBuf::from),
            // System pass only: Ollama's num_gpu option (layers on the GPU). 0 = CPU
            // only, for when another app holds the GPU memory.
            "--system-num-gpu" => {
                a.system_num_gpu = it.next().map(|v| v.parse().expect("--system-num-gpu N"))
            }
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

#[tokio::main]
async fn main() {
    if !cfg!(debug_assertions) {
        eprintln!("focus_calibration is dev-only and runs in a debug build");
        std::process::exit(2);
    }
    let args = parse_args();
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let root = home.join(".cache/qr-892-calibration");
    let run = args
        .resume
        .clone()
        .unwrap_or_else(|| chrono::Local::now().format("run-%Y%m%d-%H%M%S").to_string());
    let out_dir = root.join(&run);
    fs::create_dir_all(&out_dir).expect("create run dir");

    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("http client");
    let bs = briefs();
    let analysis_step = shipped_step("writing-assistant:voice_analysis:default");
    let draft_step = shipped_step("writing-assistant:draft_writing:default");

    let mut records: Vec<Record> = Vec::new();
    if args.resume.is_some() {
        for b in &bs {
            for m in MODELS {
                for n in 1..=SAMPLES {
                    let p = out_dir.join(b.id).join(format!("{}-{n}.json", slug(m)));
                    if let Ok(s) = fs::read_to_string(p) {
                        if let Ok(r) = serde_json::from_str::<Record>(&s) {
                            records.push(r);
                        }
                    }
                }
            }
        }
    }

    // -- Sidecar (not needed for the system pass alone) --------------------
    let mut sidecar = OllamaSidecar::new();
    let main_run_needed = !args.system_pass;
    let mut sidecar_base = String::new();
    if main_run_needed {
        let real_models = args
            .models_dir
            .clone()
            .unwrap_or_else(|| home.join(".local/share/ai.quietrabbit.app/ollama_models"));
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
        sidecar_base = format!("http://127.0.0.1:{}", ollama_sidecar::port());
        println!("sidecar: ready at {sidecar_base}");
    }

    if main_run_needed {
        let started = Instant::now();
        for (bi, b) in bs.iter().enumerate() {
            let brief_start = Instant::now();
            println!("\n== {} ({}/{})", b.title, bi + 1, bs.len());
            let voice = format_voice(&b.voice);
            let base_tokens = |analysis: &str| {
                BTreeMap::from([
                    ("user_input", b.user_input.to_owned()),
                    ("voice_profile", voice.clone()),
                    ("voice_analysis_output", analysis.to_owned()),
                    ("job_title", b.persona.job_title.to_owned()),
                    ("employer", b.persona.employer.to_owned()),
                    ("location_city", b.persona.location_city.to_owned()),
                ])
            };

            // Shared analysis step: shipped voice_analysis template, llama3.2:3b.
            warm_up(
                &http,
                &sidecar_base,
                MODELS[0],
                &options_for(&analysis_step, MODELS[0]),
            )
            .await;
            let mut analyses = Vec::new();
            for n in 1..=SAMPLES {
                let apath = out_dir.join(b.id).join(format!("analysis-{n}.md"));
                let text = if let Ok(t) = fs::read_to_string(&apath) {
                    t
                } else {
                    let prompt = render_template(&analysis_step.prompt_template, &base_tokens(""));
                    let opts = options_for(&analysis_step, MODELS[0]);
                    let t = match generate(&http, &sidecar_base, MODELS[0], &prompt, &opts).await {
                        Ok(g) => g.text.trim().to_owned(),
                        Err(e) => {
                            println!(
                                "  analysis-{n}: ERROR {e} (draft prompt gets an empty analysis)"
                            );
                            String::new()
                        }
                    };
                    write(&apath, &t);
                    t
                };
                analyses.push(text);
            }

            for m in MODELS {
                warm_up(&http, &sidecar_base, m, &options_for(&draft_step, m)).await;
                for n in 1..=SAMPLES {
                    let prompt = render_template(
                        &draft_step.prompt_template,
                        &base_tokens(&analyses[(n - 1) as usize]),
                    );
                    write(&out_dir.join(b.id).join(format!("prompt-{n}.txt")), &prompt);
                    let opts = options_for(&draft_step, m);
                    let r = run_one(&http, &sidecar_base, &out_dir, b, m, n, &prompt, &opts).await;
                    records.retain(|x| {
                        !(x.brief == r.brief && x.model == r.model && x.sample == r.sample)
                    });
                    records.push(r);
                }
            }
            compare_page(&out_dir, b, &MODELS, SAMPLES, &records);
            index_page(
                &out_dir,
                &run,
                "Writing Assistant calibration",
                &bs,
                &records,
            );

            let brief_secs = brief_start.elapsed().as_secs_f64();
            if bi == 0 {
                let first_brief_secs = brief_secs;
                let remaining = (bs.len() - 1) as f64;
                let projected = first_brief_secs * bs.len() as f64;
                println!(
                    "\nTIMING after brief 1: {:.1} min for brief 1 (analysis, warm-ups and {} drafts). \
                     Projected full run if the other briefs cost the same: {:.0} min \
                     ({:.0} min still to go). Essay and complaint briefs ask for longer output, \
                     so treat this as a floor.",
                    first_brief_secs / 60.0,
                    MODELS.len() as u32 * SAMPLES,
                    projected / 60.0,
                    first_brief_secs * remaining / 60.0
                );
                if projected > LONG_RUN_LIMIT_SECS && !args.approve_long_run {
                    println!(
                        "STOP: the projected run is over an hour. Brief 1 is saved under {}. \
                         Re-run with --resume {run} --approve-long-run to continue.",
                        out_dir.display()
                    );
                    sidecar.stop().await;
                    std::process::exit(3);
                }
            }
        }
        println!(
            "\nmain run finished in {:.1} min. Pages: {}",
            started.elapsed().as_secs_f64() / 60.0,
            out_dir.join("index.html").display()
        );
        sidecar.stop().await;
    }

    // -- Optional second pass: system Ollama, hand-run experiment ----------
    if args.system_pass {
        let sys_dir = out_dir.join("system-ollama-experiment");
        println!(
            "\nSYSTEM PASS (hand-run experiment on {SYSTEM_OLLAMA}; not the shipped path) -> {}",
            sys_dir.display()
        );
        let mut sys_records = Vec::new();
        // Own client: one request past SYSTEM_PASS_LIMIT stops that model.
        let sys_http = reqwest::Client::builder()
            .timeout(SYSTEM_PASS_LIMIT)
            .build()
            .expect("system pass http client");
        let mut stopped: Vec<&str> = Vec::new();
        for b in &bs {
            // Same prompt as sample 1 of the main run, so the prompt text is identical.
            let prompt_path = out_dir.join(b.id).join("prompt-1.txt");
            let prompt = fs::read_to_string(&prompt_path).unwrap_or_else(|_| {
                panic!("{} missing: run the main run first", prompt_path.display())
            });
            println!("\n== {}", b.title);
            for m in SYSTEM_PASS_MODELS {
                if stopped.contains(&m) {
                    println!("  {m}: stopped earlier (over 10 min), skipped");
                    continue;
                }
                let mut opts = options_for(&draft_step, m);
                opts["num_ctx"] = json!(8192);
                opts["num_predict"] = json!(SYSTEM_PASS_NUM_PREDICT);
                if let Some(n) = args.system_num_gpu {
                    opts["num_gpu"] = json!(n);
                }
                warm_up(&sys_http, SYSTEM_OLLAMA, m, &opts).await;
                let r = run_one(&sys_http, SYSTEM_OLLAMA, &sys_dir, b, m, 1, &prompt, &opts).await;
                if r.error.as_deref().is_some_and(|e| e.contains("timed out")) {
                    println!(
                        "  {m}: STOPPED, one generation passed {} min. Results so far are kept.",
                        SYSTEM_PASS_LIMIT.as_secs() / 60
                    );
                    stopped.push(m);
                }
                sys_records.push(r);
            }
            compare_page(&sys_dir, b, &SYSTEM_PASS_MODELS, 1, &sys_records);
            index_page(
                &sys_dir,
                &run,
                &match args.system_num_gpu {
                    Some(n) => format!(
                        "System Ollama experiment (hand-run, not shipped; num_gpu={n}, so latencies are not GPU latencies)"
                    ),
                    None => "System Ollama experiment (hand-run, not shipped)".to_owned(),
                },
                &bs,
                &sys_records,
            );
        }
        // Best-effort unload so the experiment leaves the system Ollama's GPU free.
        for m in SYSTEM_PASS_MODELS {
            let _ = http
                .post(format!("{SYSTEM_OLLAMA}/api/generate"))
                .json(&json!({"model": m, "keep_alive": 0}))
                .send()
                .await;
        }
        if !stopped.is_empty() {
            println!("models stopped for running past 10 min: {stopped:?}");
        }
        println!(
            "\nsystem pass finished. Pages: {}",
            sys_dir.join("index.html").display()
        );
    }
}

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

// ---------------------------------------------------------------------------
// VARIANT RUN (items.id=894 part 0). Also DEV-ONLY, and an EXPERIMENT: the prompt
// written in this file (VARIANT_TEMPLATE) is not a shipped template and nothing
// here changes one. It tests whether prompt and input changes fix two faults Jason
// read in the items.id=893 drafts: the supplied context is used inconsistently,
// and details are embellished or invented.
//   cargo run --example focus_calibration -- --variant [--compare-to <run>]
//   cargo run --example focus_calibration -- --variant --resume <variant-run>
// Same five synthetic briefs, same three bundled models, QR's own sidecar, same
// conventions under $HOME/.cache/qr-892-calibration/<run>/ (the run is named
// variant-<timestamp>). What differs from the original run, all at once:
//   - facts-only instruction (nothing not supplied; leave it out, no placeholder);
//   - no request for polish; a plain, complete draft; no remarks about the draft;
//   - the brief is a structured list of points and facts, not prose;
//   - sender name, address and date are in the synthetic persona context;
//   - the voice-analysis step is NOT run (its prose output is a loose input that
//     itself adds characterisation; the original draft prompt fed it in);
//   - temperature 0.2 for the draft (shipped 0.5); other options as shipped;
//   - four samples per brief and model (60 generations) instead of two.
// Deterministic measures per draft (no quality scoring): coverage of the brief's
// listed points (keyword matching), unsupported specifics (numbers, dates, spelled
// numbers, capitalised names and places not in the brief or persona context, listed
// for a reader to check), bracketed placeholders, opening and closing chatter, word
// count and latency. The same measures are recomputed on the original run's saved
// drafts for the summary table (summary.html / summary.md and the top of index.html).
// WHAT THE CHECKS CANNOT CATCH is printed on the pages: soft embellishment with no
// number or capitalised name ("seasoned professional"), invented causes or
// consequences, lower-case invented groups, a wrong sentence that happens to contain
// the keywords, a correct paraphrase that does not, tone, and readability.
// Latency is steady state: each model is warmed up with the same options before its
// samples (cb525cb). The original run's latency is taken from sample 2 only because
// its first sample carried a model reload.

use std::collections::{BTreeMap, HashSet};
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
// Sidecar start (shared by the original and the variant run)
// ---------------------------------------------------------------------------

/// Starts QR's own sidecar against a scratch data root whose ollama_models is a
/// symlink to the real models directory. Exits the process if it does not start.
/// Returns the base URL.
async fn start_sidecar(
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

// ---------------------------------------------------------------------------
// Variant run (items.id=894 part 0): briefs, prompt, measures
// ---------------------------------------------------------------------------

const VARIANT_SAMPLES: u32 = 4;
const VARIANT_TEMPERATURE: f64 = 0.2;
const DEFAULT_COMPARE_TO: &str = "run-20261113-194518";

/// EXPERIMENT prompt, not a shipped template. Facts-only, no polish, no remarks
/// about the draft, structured points, sender details supplied.
const VARIANT_TEMPLATE: &str = "Write a plain, complete {form} from the points below. \
Use only the points and facts supplied here. Do not add any claim, number, name, place or date \
that is not given. If something is not supplied, leave it out; do not write a placeholder such as \
[Name] or [Date]. Do not try to make it polished or impressive: plain wording is wanted. \
Output only the {form} itself, with no opening or closing remarks about it, no notes and no questions.\n\
\n\
Task: {task}\n\
Recipient: {recipient}\n\
Format: {format}\n\
\n\
Points and facts (cover every one, add nothing):\n\
{points}\n\
\n\
Sender details (use only where the format or a point calls for them):\n\
- Name: {sender_name}\n\
- Address: {sender_address}\n\
- Role: {job_title}, {employer}\n\
- Date: {date}\n\
\n\
Voice preferences: {voice_profile}\n";

struct Point {
    /// Shown in the prompt and on the pages.
    text: &'static str,
    /// Coverage test: every group must match, a group matches when any of its
    /// alternatives is found (lower case, at the start of a word) in the draft.
    keys: &'static [&'static [&'static str]],
}

struct Sender {
    name: &'static str,
    address: &'static str,
    date: &'static str,
}

/// The variant's structured form of one brief. Persona role/organisation and voice
/// come from the matching `Brief`. Every name, address and date is invented.
struct VBrief {
    id: &'static str,
    form: &'static str,
    task: &'static str,
    recipient: &'static str,
    format: &'static str,
    sender: Sender,
    points: Vec<Point>,
}

const DATE: &str = "13 November 2026";

fn variant_briefs() -> Vec<VBrief> {
    let warehouse = || Sender {
        name: "Tomas Brandvold",
        address: "418 Larch Street, Duluth, MN 55802",
        date: DATE,
    };
    let clerk = || Sender {
        name: "Renata Voss",
        address: "92 Alder Court, Spokane, WA 99201",
        date: DATE,
    };
    let library = || Sender {
        name: "Ilse Marchetti",
        address: "7 Quarry Lane, Bozeman, MT 59715",
        date: DATE,
    };
    vec![
        VBrief {
            id: "rma-letter",
            form: "letter",
            task: "Ask for a return authorization for a laptop.",
            recipient: "Returns department, Brightwater Electronics",
            format: "Business letter: sender's name and address, date, recipient, salutation, body, sign-off with the sender's name.",
            sender: warehouse(),
            points: vec![
                Point { text: "Wants a return authorization", keys: &[&["return authorization", "return authorisation", "return merchandise", "rma"]] },
                Point { text: "Product: Vantelle X14 laptop", keys: &[&["vantelle x14"]] },
                Point { text: "Serial number: VX14-0042718", keys: &[&["vx14-0042718"]] },
                Point { text: "Bought on 3 August 2026 from Brightwater Electronics", keys: &[&["3 august 2026", "august 3, 2026", "august 3 2026", "august 3rd", "3rd august"]] },
                Point { text: "Order number: BW-558201", keys: &[&["bw-558201"]] },
                Point { text: "The left hinge cracked in the second week", keys: &[&["hinge"], &["second week", "2nd week", "two weeks", "week two"]] },
                Point { text: "The screen now wobbles", keys: &[&["wobble"]] },
                Point { text: "Wants a replacement, not a refund", keys: &[&["replacement"], &["refund"]] },
                Point { text: "Can ship it at own cost if they send a label", keys: &[&["label"], &["own cost", "own expense", "my expense", "my cost", "at my own", "pay for", "pay the", "cover the", "cover shipping", "shipping cost", "shipping expense", "shipping fee", "postage"]] },
            ],
        },
        VBrief {
            id: "cover-letter",
            form: "cover letter",
            task: "Apply for a job as logistics coordinator.",
            recipient: "Northgate Freight Partners, Tacoma",
            format: "Cover letter: sender's name and address, date, salutation, body, sign-off with the sender's name.",
            sender: warehouse(),
            points: vec![
                Point { text: "Applying for the logistics coordinator job at Northgate Freight Partners in Tacoma", keys: &[&["logistics coordinator"], &["northgate"]] },
                Point { text: "Six years in warehouse operations", keys: &[&["six years", "6 years"], &["warehouse"]] },
                Point { text: "Cut mis-picks by a third at the current job", keys: &[&["mis-pick", "mispick", "mis pick"], &["a third", "one-third", "one third"]] },
                Point { text: "Did it by redoing the bin labelling", keys: &[&["bin label"]] },
                Point { text: "Taught themselves spreadsheet reporting", keys: &[&["spreadsheet"], &["taught", "self-taught", "teach"]] },
                Point { text: "The team stopped doing the reporting by hand", keys: &[&["by hand", "manual"]] },
                Point { text: "Wants to move into planning work", keys: &[&["planning"]] },
            ],
        },
        VBrief {
            id: "complaint-email",
            form: "email",
            task: "Make a formal complaint.",
            recipient: "Customer service manager, Corvid Broadband",
            format: "Email: a Subject line, salutation, body, sign-off with the sender's name. No address block or date line.",
            sender: clerk(),
            points: vec![
                Point { text: "Router: TR-220", keys: &[&["tr-220"]] },
                Point { text: "The connection has dropped every evening", keys: &[&["evening", "night"], &["drop", "disconnect", "cut out", "lost", "interrupt", "unstable", "intermittent"]] },
                Point { text: "For five weeks", keys: &[&["five weeks", "5 weeks", "five-week"]] },
                Point { text: "Four support tickets logged", keys: &[&["four", "4 ticket", "4 support", "4 separate"], &["ticket"]] },
                Point { text: "Ticket numbers: CB-77120, CB-77391, CB-77804, CB-78213", keys: &[&["cb-77120"], &["cb-77391"], &["cb-77804"], &["cb-78213"]] },
                Point { text: "Told each time to restart it", keys: &[&["restart"]] },
                Point { text: "A technician visit on 14 October never happened", keys: &[&["technician"], &["14 october", "october 14"]] },
                Point { text: "Nobody called", keys: &[&["nobody", "no one", "never called", "did not call", "didn't call", "no call", "not called", "no phone call", "without calling", "failed to call", "follow-up", "follow up", "not contacted", "never contacted", "no contact", "no communication", "not received any", "was not contacted", "wasn't contacted"]] },
                Point { text: "Wants the fault fixed", keys: &[&["fix", "repair", "resolv", "rectif"]] },
                Point { text: "Wants a credit for the weeks paid for service not received", keys: &[&["credit"]] },
                Point { text: "Wants a named contact", keys: &[&["named", "dedicated", "direct contact", "contact person", "point of contact"]] },
            ],
        },
        VBrief {
            id: "personal-note",
            form: "note",
            task: "Thank a neighbour.",
            recipient: "Dorrie, a neighbour",
            format: "Short personal note of a few lines: salutation and sign-off with the sender's first name. No address block or date line.",
            sender: clerk(),
            points: vec![
                Point { text: "Thanks to Dorrie", keys: &[&["dorrie"], &["thank"]] },
                Point { text: "She kept an eye on the house", keys: &[&["house"], &["eye", "watch", "look", "check"]] },
                Point { text: "She watered the tomatoes", keys: &[&["tomato"], &["water"]] },
                Point { text: "They were away for ten days", keys: &[&["ten days", "10 days"]] },
                Point { text: "They left her a jar of lemon curd", keys: &[&["lemon curd"]] },
                Point { text: "She liked the lemon curd", keys: &[&["liked", "enjoy", "love", "favourite", "favorite"]] },
            ],
        },
        VBrief {
            id: "essay-section",
            form: "essay section",
            task: "Write one section of an essay.",
            recipient: "General readers of the essay",
            format: "One essay section of about one page: plain argument in paragraphs, no list, no heading, no letterhead, no sign-off.",
            sender: library(),
            points: vec![
                Point { text: "Argument: small towns should keep their paper-based public notice boards", keys: &[&["notice board", "notice-board", "bulletin board"], &["paper"]] },
                Point { text: "This holds even when the council also posts notices online", keys: &[&["council"], &["online"]] },
                Point { text: "Cover who gets left out by online-only notices", keys: &[&["left out", "leave out", "leaves out", "excluded", "exclude", "miss out", "shut out", "locked out", "cut off", "no access", "cannot access", "can't access", "unable to access", "no internet", "without internet", "without access", "lack access", "lack internet", "don't have access", "do not have access"]] },
                Point { text: "A board in the library or post office is read by people who never visit the council website", keys: &[&["library"], &["post office"]] },
                Point { text: "Those readers never visit the council website", keys: &[&["website", "web site"]] },
            ],
        },
    ]
}

fn variant_options(step: &ShippedStep, model: &str) -> Value {
    let mut o = options_for(step, model);
    o["temperature"] = json!(VARIANT_TEMPERATURE);
    o
}

fn variant_prompt(b: &Brief, vb: &VBrief) -> String {
    let points = vb
        .points
        .iter()
        .map(|p| format!("- {}", p.text))
        .collect::<Vec<_>>()
        .join("\n");
    let tokens = BTreeMap::from([
        ("form", vb.form.to_owned()),
        ("task", vb.task.to_owned()),
        ("recipient", vb.recipient.to_owned()),
        ("format", vb.format.to_owned()),
        ("points", points),
        ("sender_name", vb.sender.name.to_owned()),
        ("sender_address", vb.sender.address.to_owned()),
        ("job_title", b.persona.job_title.to_owned()),
        ("employer", b.persona.employer.to_owned()),
        ("date", vb.sender.date.to_owned()),
        ("voice_profile", format_voice(&b.voice)),
    ]);
    render_template(VARIANT_TEMPLATE, &tokens)
}

/// The text a draft may draw its specifics from. Original run: the prose request
/// and the persona context it had. Variant: the structured brief and the persona
/// context it had, which adds the sender name, address and date.
fn allowed_text(b: &Brief, vb: &VBrief, variant: bool) -> String {
    if !variant {
        return format!(
            "{} {} {} {}",
            b.user_input, b.persona.job_title, b.persona.employer, b.persona.location_city
        );
    }
    let mut s = format!(
        "{} {} {} {} {} {} {} {}",
        vb.task,
        vb.recipient,
        vb.sender.name,
        vb.sender.address,
        vb.sender.date,
        b.persona.job_title,
        b.persona.employer,
        b.persona.location_city
    );
    for p in &vb.points {
        s.push(' ');
        s.push_str(p.text);
    }
    s
}

// -- Deterministic measures -------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
struct Measures {
    points_total: usize,
    points_covered: usize,
    missing_points: Vec<String>,
    /// Numbers, dates, spelled numbers and capitalised names or places that are
    /// not in the brief or persona context. A list for a reader to check.
    unsupported: Vec<String>,
    placeholders: Vec<String>,
    opening_chatter: Option<String>,
    closing_chatter: Option<String>,
}

/// Lower-cases and folds typographic quotes, dashes and no-break spaces, so the
/// keyword tests are not defeated by them.
fn norm(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{2010}'..='\u{2015}' => '-',
            '\u{00a0}' => ' ',
            c => c,
        })
        .collect::<String>()
        .to_lowercase()
}

/// `term` (already lower case) occurs in `hay` at the start of a word.
fn has_term(hay: &str, term: &str) -> bool {
    hay.match_indices(term).any(|(i, _)| {
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

fn point_covered(hay: &str, keys: &[&[&str]]) -> bool {
    keys.iter()
        .all(|alts| alts.iter().any(|a| has_term(hay, a)))
}

/// Bracketed placeholders such as [Your Name], {date} or <Name>. Returns the spans
/// and the text with each span replaced by a space. A bracket pair with no letters
/// inside (a citation like [1]) or that starts with white space is not a span.
fn split_placeholders(text: &str) -> (Vec<String>, String) {
    let mut spans: Vec<String> = Vec::new();
    let mut rest = String::with_capacity(text.len());
    for (li, line) in text.lines().enumerate() {
        if li > 0 {
            rest.push('\n');
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let close = match chars[i] {
                '[' => ']',
                '{' => '}',
                '<' => '>',
                c => {
                    rest.push(c);
                    i += 1;
                    continue;
                }
            };
            let end = chars[i + 1..].iter().take(80).position(|&c| c == close);
            if let Some(e) = end {
                let inner: String = chars[i + 1..i + 1 + e].iter().collect();
                if inner.chars().any(char::is_alphabetic)
                    && !inner.starts_with(char::is_whitespace)
                    && !inner.starts_with('/')
                {
                    spans.push(chars[i..i + e + 2].iter().collect());
                    rest.push(' ');
                    i += e + 2;
                    continue;
                }
            }
            rest.push(chars[i]);
            i += 1;
        }
    }
    (spans, rest)
}

fn starts_with_word(s: &str, w: &str) -> bool {
    s.starts_with(w)
        && s[w.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric())
}

/// First non-empty line is remarks about the draft rather than the draft.
fn opening_chatter(text: &str) -> Option<String> {
    const STARTS: &[&str] = &[
        "here is",
        "here's",
        "here are",
        "here you",
        "sure",
        "certainly",
        "of course",
        "absolutely",
        "okay",
        "ok",
        "below is",
        "i've written",
        "i have written",
        "i've drafted",
        "i have drafted",
        "i've put together",
        "happy to",
        "great",
    ];
    const WORDS: &[&str] = &[
        "draft", "letter", "email", "note", "essay", "section", "version",
    ];
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let low = norm(first);
    let low = low.trim_start_matches(|c: char| "*#>-_ ".contains(c));
    let starts = STARTS.iter().any(|s| starts_with_word(low, s));
    let colon = low.ends_with(':')
        && !low.starts_with("dear")
        && WORDS.iter().any(|w| low.contains(w))
        && low.split_whitespace().count() <= 14;
    (starts || colon).then(|| first.to_owned())
}

/// The last paragraph talks about the draft itself (an offer to revise, a note).
fn closing_chatter(text: &str) -> Option<String> {
    const PHRASES: &[&str] = &[
        "this draft",
        "this letter is",
        "this email is",
        "this note is",
        "i hope this helps",
        "hope this helps",
        "let me know if you'd like me",
        "let me know if you want me",
        "let me know if you would like me",
        "feel free to adjust",
        "feel free to modify",
        "feel free to customize",
        "feel free to customise",
        "you can adjust",
        "you may want to",
        "note:",
        "(note",
        "adjust as needed",
        "if you'd like me to",
        "if you would like me to",
        "would you like me to",
        "i can adjust",
        "i can revise",
    ];
    let last = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .last()?;
    let low = norm(last);
    PHRASES
        .iter()
        .any(|p| low.contains(p))
        .then(|| last.lines().next().unwrap_or(last).to_owned())
}

/// Capitalised words that are fine anywhere (greetings, closings, headings).
const COMMON_CAPS: &[&str] = &[
    "dear",
    "hello",
    "hi",
    "hey",
    "sincerely",
    "regards",
    "best",
    "thanks",
    "thank",
    "you",
    "yours",
    "kind",
    "kindly",
    "warm",
    "warmly",
    "cheers",
    "respectfully",
    "faithfully",
    "truly",
    "subject",
    "re",
    "sir",
    "madam",
    "to",
    "from",
    "date",
    "cc",
    "please",
    "mr",
    "mrs",
    "ms",
    "miss",
    "dr",
    "mx",
    "regarding",
    "cordially",
    "gratefully",
    "with",
    "and",
    "the",
    "of",
    "for",
    "in",
    "on",
    "at",
    "a",
    "an",
    "hiring",
    "manager",
    "team",
    "committee",
];
const HONORIFICS: &[&str] = &["mr", "mrs", "ms", "dr", "miss", "mx", "st"];
const NUMBER_WORDS: &[&str] = &[
    "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve",
    "twenty", "thirty", "forty", "fifty", "hundred", "thousand", "dozen", "half", "third",
    "quarter",
];

fn word_set(text: &str) -> HashSet<String> {
    norm(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every alphanumeric part of `core` (split at hyphens, slashes and so on) is a
/// word of the allowed text. So VX14-0042718 passes when both parts are supplied
/// and VX14-0042719 does not.
fn parts_allowed(core: &str, allowed: &HashSet<String>) -> bool {
    norm(core)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .all(|w| allowed.contains(w))
}

fn core_token(t: &str) -> &str {
    t.trim_matches(|c: char| !c.is_alphanumeric())
}

fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(char::is_uppercase)
}

fn ends_sentence(t: &str) -> bool {
    t.trim_end_matches(['"', '\'', ')', '*', '_', '\u{201d}', '\u{2019}'])
        .ends_with(['.', '!', '?', ':'])
}

/// Numbers, dates, spelled numbers and capitalised names or places in the draft
/// that are not in the allowed text. Heuristic, built to be listed for a person to
/// check, not to be exact: it has false positives (a title-case word in a heading,
/// a product word) and false negatives (see the page text). Placeholders are cut out
/// first (counted separately). A single capitalised word that opens a sentence is
/// skipped unless it has a digit, an inner capital, or sits on a short all-capitalised
/// line (a name or greeting line); a Subject line is checked for numbers only.
fn unsupported_specifics(text: &str, allowed: &HashSet<String>) -> Vec<String> {
    let (_, text) = split_placeholders(text);
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let Some(first) = toks.first() else { continue };
        let first_low = norm(core_token(first));
        let heading = first_low == "subject" || first_low == "re";
        let name_line = toks.len() <= 3 && toks.iter().all(|t| starts_upper(core_token(t)));
        let mut sentence_start = true;
        for (i, t) in toks.iter().enumerate() {
            let core = core_token(t);
            let initial = sentence_start;
            let low = norm(core);
            sentence_start = !HONORIFICS.contains(&low.as_str()) && ends_sentence(t);
            if core.is_empty() {
                continue;
            }
            let digits_only = core.chars().all(|c| c.is_ascii_digit());
            if i == 0 && digits_only && (t.ends_with('.') || t.ends_with(')')) {
                sentence_start = true; // a list marker such as "1."
                continue;
            }
            let mut flag = None;
            if core.chars().any(|c| c.is_ascii_digit()) {
                if !parts_allowed(core, allowed) {
                    flag = Some(core);
                }
            } else if heading {
                continue;
            } else if starts_upper(core) {
                let p = core.split(['\'', '\u{2019}']).next().unwrap_or(core);
                let common = norm(p)
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .all(|w| COMMON_CAPS.contains(&w));
                let inner_caps = p.chars().skip(1).any(char::is_uppercase);
                if p.chars().count() > 1
                    && !common
                    && !(initial && !name_line && !inner_caps)
                    && !parts_allowed(p, allowed)
                {
                    flag = Some(p);
                }
            } else if NUMBER_WORDS.contains(&low.as_str()) && !allowed.contains(&low) {
                flag = Some(core);
            }
            if let Some(f) = flag {
                if !out.iter().any(|x| x == f) {
                    out.push(f.to_owned());
                }
            }
        }
    }
    out
}

fn measure(text: &str, points: &[Point], allowed: &HashSet<String>) -> Measures {
    let hay = norm(text);
    let missing: Vec<String> = points
        .iter()
        .filter(|p| !point_covered(&hay, p.keys))
        .map(|p| p.text.to_owned())
        .collect();
    Measures {
        points_total: points.len(),
        points_covered: points.len() - missing.len(),
        missing_points: missing,
        unsupported: unsupported_specifics(text, allowed),
        placeholders: split_placeholders(text).0,
        opening_chatter: opening_chatter(text),
        closing_chatter: closing_chatter(text),
    }
}

/// One saved draft with its generation record and measures.
struct Draft {
    brief: String,
    model: String,
    sample: u32,
    rec: Record,
    text: String,
    m: Measures,
}

/// Reads every saved draft under `dir` and measures it. `variant` picks which
/// allowed text a draft is checked against.
fn load_drafts(
    dir: &Path,
    bs: &[Brief],
    vbs: &[VBrief],
    samples: u32,
    variant: bool,
) -> Vec<Draft> {
    let mut out = Vec::new();
    for (b, vb) in bs.iter().zip(vbs) {
        assert_eq!(b.id, vb.id, "briefs and variant briefs are out of step");
        let allowed = word_set(&allowed_text(b, vb, variant));
        for m in MODELS {
            for n in 1..=samples {
                let stem = format!("{}-{n}", slug(m));
                let base = dir.join(b.id);
                let (Ok(js), Ok(text)) = (
                    fs::read_to_string(base.join(format!("{stem}.json"))),
                    fs::read_to_string(base.join(format!("{stem}.md"))),
                ) else {
                    continue;
                };
                let Ok(rec) = serde_json::from_str::<Record>(&js) else {
                    continue;
                };
                let measures = measure(&text, &vb.points, &allowed);
                out.push(Draft {
                    brief: b.id.to_owned(),
                    model: (*m).to_owned(),
                    sample: n,
                    rec,
                    text,
                    m: measures,
                });
            }
        }
    }
    out
}

// -- Summary ----------------------------------------------------------------

struct Stats {
    n: usize,
    cov_covered: usize,
    cov_total: usize,
    all_cov: usize,
    unsup_total: usize,
    unsup_any: usize,
    ph_any: usize,
    ph_total: usize,
    open: usize,
    close: usize,
    words_mean: f64,
    lat_median_s: f64,
    spread2: f64,
    spread_all: f64,
}

/// Mean over briefs of (max - min) points covered across the samples of that brief,
/// using samples up to `max_sample`.
fn spread(ds: &[&Draft], max_sample: u32) -> f64 {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for d in ds.iter().filter(|d| d.sample <= max_sample) {
        let c = d.m.points_covered;
        let e = by.entry(d.brief.as_str()).or_insert((c, c));
        e.0 = e.0.min(c);
        e.1 = e.1.max(c);
    }
    if by.is_empty() {
        0.0
    } else {
        by.values().map(|(lo, hi)| (hi - lo) as f64).sum::<f64>() / by.len() as f64
    }
}

/// `latency_sample`: when set, only drafts of that sample number count for latency.
fn stats(ds: &[&Draft], latency_sample: Option<u32>) -> Stats {
    let mut lat: Vec<f64> = ds
        .iter()
        .filter(|d| latency_sample.is_none_or(|s| d.sample == s))
        .map(|d| d.rec.latency_ms / 1000.0)
        .collect();
    lat.sort_by(|a, b| a.total_cmp(b));
    let lat_median_s = match lat.len() {
        0 => 0.0,
        l if l % 2 == 1 => lat[l / 2],
        l => (lat[l / 2 - 1] + lat[l / 2]) / 2.0,
    };
    Stats {
        n: ds.len(),
        cov_covered: ds.iter().map(|d| d.m.points_covered).sum(),
        cov_total: ds.iter().map(|d| d.m.points_total).sum(),
        all_cov: ds
            .iter()
            .filter(|d| d.m.points_covered == d.m.points_total)
            .count(),
        unsup_total: ds.iter().map(|d| d.m.unsupported.len()).sum(),
        unsup_any: ds.iter().filter(|d| !d.m.unsupported.is_empty()).count(),
        ph_any: ds.iter().filter(|d| !d.m.placeholders.is_empty()).count(),
        ph_total: ds.iter().map(|d| d.m.placeholders.len()).sum(),
        open: ds.iter().filter(|d| d.m.opening_chatter.is_some()).count(),
        close: ds.iter().filter(|d| d.m.closing_chatter.is_some()).count(),
        words_mean: ds.iter().map(|d| d.rec.words as f64).sum::<f64>() / ds.len().max(1) as f64,
        lat_median_s,
        spread2: spread(ds, 2),
        spread_all: spread(ds, u32::MAX),
    }
}

const SUMMARY_HEADER: [&str; 14] = [
    "Model",
    "Run",
    "Drafts",
    "Brief points covered",
    "Drafts with every point",
    "Unsupported specifics per draft",
    "Drafts with 1+ unsupported",
    "Drafts with a placeholder (placeholders)",
    "Opening chatter",
    "Closing chatter",
    "Mean words",
    "Median latency (s)",
    "Coverage spread, first 2 samples (points)",
    "Coverage spread, all samples (points)",
];

fn stats_row(model: &str, run: &str, s: &Stats) -> Vec<String> {
    let n = s.n.max(1) as f64;
    vec![
        model.to_owned(),
        run.to_owned(),
        s.n.to_string(),
        format!(
            "{}/{} ({:.0}%)",
            s.cov_covered,
            s.cov_total,
            100.0 * s.cov_covered as f64 / s.cov_total.max(1) as f64
        ),
        format!("{}/{}", s.all_cov, s.n),
        format!("{:.2}", s.unsup_total as f64 / n),
        format!("{}/{}", s.unsup_any, s.n),
        format!("{}/{} ({})", s.ph_any, s.n, s.ph_total),
        format!("{}/{}", s.open, s.n),
        format!("{}/{}", s.close, s.n),
        format!("{:.0}", s.words_mean),
        format!("{:.1}", s.lat_median_s),
        format!("{:.2}", s.spread2),
        format!("{:.2}", s.spread_all),
    ]
}

/// Rows: per model, the original run then the variant, over the briefs the variant
/// run has finished. Original latency is sample 2 only (sample 1 carried a reload).
fn summary_rows(orig: &[Draft], var: &[Draft]) -> Vec<Vec<String>> {
    let done: HashSet<&str> = var.iter().map(|d| d.brief.as_str()).collect();
    let mut rows = Vec::new();
    for m in MODELS {
        let o: Vec<&Draft> = orig
            .iter()
            .filter(|d| d.model == m && d.rec.error.is_none() && done.contains(d.brief.as_str()))
            .collect();
        let v: Vec<&Draft> = var
            .iter()
            .filter(|d| d.model == m && d.rec.error.is_none())
            .collect();
        rows.push(stats_row(
            m,
            "original (2 samples, temp 0.5)",
            &stats(&o, Some(2)),
        ));
        rows.push(stats_row(
            m,
            "variant (4 samples, temp 0.2)",
            &stats(&v, None),
        ));
    }
    rows
}

const SUMMARY_NOTES: &[&str] = &[
    "Both runs: the same five synthetic briefs, the same three bundled models, QR's own sidecar. The measures were recomputed on the saved original drafts with the same code, so the two are comparable.",
    "The variant changed several things at once: facts-only instruction, no request for polish, no remarks about the draft, a structured list of points, sender name, address and date supplied, no voice-analysis step, temperature 0.2. This table cannot say which change did what.",
    "Samples: original 2 per brief and model (30 drafts), variant 4 (60). Coverage spread is max minus min of points covered across the samples of one brief, averaged over briefs. It grows with the number of samples, so compare the first-2-samples column across the two runs.",
    "Latency: the variant is steady state (warm-up with the same options before each model's samples). Original latency uses sample 2 only, because its first sample included a model reload. The variant does not make the shared voice-analysis call that fed the original draft prompt.",
    "Unsupported specifics are checked against the brief and persona context supplied in that run. The variant's context includes a sender name, address and date that the original had none of, so a name in an original draft counts as unsupported while the same supplied name in a variant draft does not.",
];

const LIMITS: &[&str] = &[
    "Soft embellishment that carries no number or capitalised name: praise and filler such as \"seasoned professional\" or \"dedicated team player\", invented reasons or consequences (\"rendering the laptop unusable\"), claims about warranty, urgency or what the reader wants, and invented groups written in lower case (\"elderly residents\"). A reader has to find these.",
    "Meaning, not words: coverage is keyword matching, so a correct paraphrase of a point can count as missing, and a wrong sentence that happens to contain the keywords counts as covered. A number is matched by value anywhere in the brief, not by what it refers to.",
    "A single invented capitalised word that opens a sentence (and is not on a short name line) is skipped on purpose; a title-case word in a heading or a product word can be listed although it is harmless. The list is for a person to check, not a score.",
    "Chatter is found by a phrase list on the first line and the last paragraph. Tone, voice, how well it reads, and whether the sender details were used where the format calls for them are not measured.",
];

fn md_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = format!(
        "| {} |\n|{}\n",
        header.join(" | "),
        "---|".repeat(header.len())
    );
    for r in rows {
        s.push_str(&format!("| {} |\n", r.join(" | ")));
    }
    s
}

fn html_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut h = String::from("<table><tr>");
    for c in header {
        h.push_str(&format!("<th>{}</th>", esc(c)));
    }
    h.push_str("</tr>");
    for r in rows {
        h.push_str("<tr>");
        for c in r {
            h.push_str(&format!("<td>{}</td>", esc(c)));
        }
        h.push_str("</tr>");
    }
    h.push_str("</table>");
    h
}

fn summary_md(rows: &[Vec<String>], compare_to: &str, run: &str) -> String {
    let mut s = format!(
        "# Variant run {run} vs original {compare_to}\n\n{}\n",
        md_table(&SUMMARY_HEADER, rows)
    );
    s.push_str("Notes:\n");
    for n in SUMMARY_NOTES {
        s.push_str(&format!("- {n}\n"));
    }
    s.push_str("\nWhat the deterministic checks cannot catch:\n");
    for n in LIMITS {
        s.push_str(&format!("- {n}\n"));
    }
    s
}

fn summary_html_block(rows: &[Vec<String>]) -> String {
    let mut h = String::from("<h2>Variant vs original</h2>");
    h.push_str(&html_table(&SUMMARY_HEADER, rows));
    h.push_str("<ul>");
    for n in SUMMARY_NOTES {
        h.push_str(&format!("<li>{}</li>", esc(n)));
    }
    h.push_str("</ul><h2>What the deterministic checks cannot catch</h2><ul>");
    for n in LIMITS {
        h.push_str(&format!("<li>{}</li>", esc(n)));
    }
    h.push_str("</ul>");
    h
}

// -- Pages ------------------------------------------------------------------

fn variant_meta(d: &Draft) -> String {
    let m = &d.m;
    let mut s = format!(
        "{} words &middot; {:.1} s &middot; coverage {}/{} &middot; placeholders {} &middot; opening chatter {} &middot; closing chatter {}",
        d.rec.words,
        d.rec.latency_ms / 1000.0,
        m.points_covered,
        m.points_total,
        m.placeholders.len(),
        if m.opening_chatter.is_some() { "<span class=warn>yes</span>" } else { "no" },
        if m.closing_chatter.is_some() { "<span class=warn>yes</span>" } else { "no" },
    );
    if d.rec.hit_cap {
        s.push_str(" &middot; <span class=warn>stopped at the 1024-token cap</span>");
    }
    if let Some(e) = &d.rec.error {
        s.push_str(&format!(
            " &middot; <span class=warn>ERROR {}</span>",
            esc(e)
        ));
    }
    s
}

fn variant_compare_page(
    out_dir: &Path,
    b: &Brief,
    vb: &VBrief,
    drafts: &[Draft],
    compare_to: &str,
) {
    let points: String = vb
        .points
        .iter()
        .map(|p| format!("<li>{}</li>", esc(p.text)))
        .collect();
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{t} variant comparison</title><style>{PAGE_CSS}</style><h1>{t} &middot; variant</h1>\
         <p class=note>Synthetic brief. VARIANT run (items.id=894 part 0, dev-only experiment): facts-only prompt, \
         structured points, sender details supplied, temperature 0.2, four samples. Measures are deterministic and \
         listed so you can check them; nothing is scored. They cannot catch soft embellishment (\"seasoned professional\"), \
         invented reasons, or a wrong sentence that contains the keywords. \
         <a href=\"../../{compare_to}/{id}/compare.html\">Same brief in the original run</a>.</p>\
         <details open><summary>Points and facts supplied</summary><ul>{points}</ul>\
         <p class=meta>Task: {task} Recipient: {rec}. Format: {fmt} Sender: {name}, {addr}, {date}. \
         Role: {job}, {emp}. Voice: {voice}.</p></details>\
         <p><a href=\"../index.html\">Index and summary</a> &middot; <a href=\"prompt.txt\">prompt</a></p><div class=grid>",
        t = esc(b.title),
        id = b.id,
        task = esc(vb.task),
        rec = esc(vb.recipient),
        fmt = esc(vb.format),
        name = esc(vb.sender.name),
        addr = esc(vb.sender.address),
        date = esc(vb.sender.date),
        job = esc(b.persona.job_title),
        emp = esc(b.persona.employer),
        voice = esc(&format_voice(&b.voice)),
    );
    for n in 1..=VARIANT_SAMPLES {
        for m in MODELS {
            let Some(d) = drafts
                .iter()
                .find(|d| d.brief == b.id && d.model == m && d.sample == n)
            else {
                h.push_str(&format!(
                    "<div class=cell><h2>{} &middot; sample {n}</h2><p class=meta>not run</p></div>",
                    esc(m)
                ));
                continue;
            };
            let missing = if d.m.missing_points.is_empty() {
                "none".to_owned()
            } else {
                d.m.missing_points
                    .iter()
                    .map(|p| esc(p))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            let unsupported = if d.m.unsupported.is_empty() {
                "none".to_owned()
            } else {
                format!(
                    "<span class=warn>{}</span>",
                    d.m.unsupported
                        .iter()
                        .map(|u| esc(u))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let placeholders = if d.m.placeholders.is_empty() {
                String::new()
            } else {
                format!(
                    "<br>Placeholders: <span class=warn>{}</span>",
                    d.m.placeholders
                        .iter()
                        .map(|p| esc(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let chatter = [
                d.m.opening_chatter
                    .as_ref()
                    .map(|c| format!("Opening: {}", esc(c))),
                d.m.closing_chatter
                    .as_ref()
                    .map(|c| format!("Closing: {}", esc(c))),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" &middot; ");
            h.push_str(&format!(
                "<div class=cell><h2>{} &middot; sample {n}</h2><p class=meta>{}</p>\
                 <p class=meta>Points not found: {missing}<br>Unsupported specifics (check these): {unsupported}{placeholders}{}</p><pre>{}</pre></div>",
                esc(m),
                variant_meta(d),
                if chatter.is_empty() { String::new() } else { format!("<br>{chatter}") },
                esc(&d.text)
            ));
        }
    }
    h.push_str("</div>");
    write(&out_dir.join(b.id).join("compare.html"), &h);
}

fn variant_index_page(
    out_dir: &Path,
    run: &str,
    bs: &[Brief],
    drafts: &[Draft],
    summary_block: &str,
    compare_to: &str,
) {
    let mut h = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Writing Assistant calibration variant</title><style>{PAGE_CSS}</style>\
         <h1>Writing Assistant calibration, variant &middot; {}</h1>\
         <p class=note>Dev-only experiment (items.id=894 part 0, not a shipped template). Compared with the original run \
         {}. Open a brief to read the drafts side by side. Measures are deterministic, nothing is a quality score. \
         Also saved as summary.md.</p>{summary_block}",
        esc(run),
        esc(compare_to)
    );
    for b in bs {
        let ds: Vec<&Draft> = drafts.iter().filter(|d| d.brief == b.id).collect();
        if ds.is_empty() {
            continue;
        }
        h.push_str(&format!(
            "<h2><a href=\"{0}/compare.html\">{1}</a></h2><table><tr><th>Model</th><th>Sample</th><th>Words</th>\
             <th>Latency</th><th>Coverage</th><th>Unsupported specifics</th><th>Placeholders</th>\
             <th>Opening chatter</th><th>Closing chatter</th><th>Notes</th></tr>",
            b.id,
            esc(b.title)
        ));
        for d in ds {
            let mut notes = Vec::new();
            if d.rec.hit_cap {
                notes.push("token cap".to_owned());
            }
            if let Some(e) = &d.rec.error {
                notes.push(format!("ERROR {}", esc(e)));
            }
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1} s</td><td>{}/{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&d.model),
                d.sample,
                d.rec.words,
                d.rec.latency_ms / 1000.0,
                d.m.points_covered,
                d.m.points_total,
                if d.m.unsupported.is_empty() {
                    "none".to_owned()
                } else {
                    format!("{}: {}", d.m.unsupported.len(), esc(&d.m.unsupported.join(", ")))
                },
                d.m.placeholders.len(),
                if d.m.opening_chatter.is_some() { "yes" } else { "no" },
                if d.m.closing_chatter.is_some() { "yes" } else { "no" },
                notes.join(", ")
            ));
        }
        h.push_str("</table>");
    }
    write(&out_dir.join("index.html"), &h);
}

/// Rewrites every derived file (measures json, compare pages, summary, index) from
/// the drafts on disk. Called after each brief, so a stopped run still has pages.
fn write_variant_outputs(
    out_dir: &Path,
    run: &str,
    compare_to: &str,
    bs: &[Brief],
    vbs: &[VBrief],
    orig: &[Draft],
) -> String {
    let drafts = load_drafts(out_dir, bs, vbs, VARIANT_SAMPLES, true);
    for d in &drafts {
        write(
            &out_dir
                .join(&d.brief)
                .join(format!("{}-{}.measures.json", slug(&d.model), d.sample)),
            &serde_json::to_string_pretty(&d.m).expect("serialize measures"),
        );
    }
    for (b, vb) in bs.iter().zip(vbs) {
        if drafts.iter().any(|d| d.brief == b.id) {
            variant_compare_page(out_dir, b, vb, &drafts, compare_to);
        }
    }
    let rows = summary_rows(orig, &drafts);
    let md = summary_md(&rows, compare_to, run);
    write(&out_dir.join("summary.md"), &md);
    variant_index_page(
        out_dir,
        run,
        bs,
        &drafts,
        &summary_html_block(&rows),
        compare_to,
    );
    md
}

async fn run_variant(args: Args, home: PathBuf, root: PathBuf) {
    let run = args.resume.clone().unwrap_or_else(|| {
        chrono::Local::now()
            .format("variant-%Y%m%d-%H%M%S")
            .to_string()
    });
    let compare_to = args
        .compare_to
        .clone()
        .unwrap_or_else(|| DEFAULT_COMPARE_TO.to_owned());
    let orig_dir = root.join(&compare_to);
    assert!(
        orig_dir.is_dir(),
        "original run {} not found (--compare-to)",
        orig_dir.display()
    );
    let out_dir = root.join(&run);
    fs::create_dir_all(&out_dir).expect("create run dir");

    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("http client");
    let bs = briefs();
    let vbs = variant_briefs();
    let draft_step = shipped_step("writing-assistant:draft_writing:default");
    let orig = load_drafts(&orig_dir, &bs, &vbs, SAMPLES, false);
    println!(
        "original run {compare_to}: {} saved drafts measured for the summary",
        orig.len()
    );

    let mut sidecar = OllamaSidecar::new();
    let base = start_sidecar(&mut sidecar, &home, &root, args.models_dir.clone()).await;

    let started = Instant::now();
    for (bi, (b, vb)) in bs.iter().zip(&vbs).enumerate() {
        let brief_start = Instant::now();
        println!("\n== {} ({}/{})", b.title, bi + 1, bs.len());
        let prompt = variant_prompt(b, vb);
        write(&out_dir.join(b.id).join("prompt.txt"), &prompt);
        for m in MODELS {
            let opts = variant_options(&draft_step, m);
            let finished = (1..=VARIANT_SAMPLES).all(|n| {
                out_dir
                    .join(b.id)
                    .join(format!("{}-{n}.json", slug(m)))
                    .exists()
            });
            if !finished {
                warm_up(&http, &base, m, &opts).await;
            }
            for n in 1..=VARIANT_SAMPLES {
                run_one(&http, &base, &out_dir, b, m, n, &prompt, &opts).await;
            }
        }
        write_variant_outputs(&out_dir, &run, &compare_to, &bs, &vbs, &orig);

        if bi == 0 {
            let first = brief_start.elapsed().as_secs_f64();
            let projected = first * bs.len() as f64;
            println!(
                "\nTIMING after brief 1: {:.1} min for brief 1 ({} drafts after warm-ups). Projected full run if the \
                 other briefs cost the same: {:.0} min ({:.0} min still to go). The essay and complaint briefs ask for \
                 longer output, so treat this as a floor.",
                first / 60.0,
                MODELS.len() as u32 * VARIANT_SAMPLES,
                projected / 60.0,
                first * (bs.len() - 1) as f64 / 60.0
            );
            if projected > LONG_RUN_LIMIT_SECS && !args.approve_long_run {
                println!(
                    "STOP: the projected run is over an hour. Brief 1 is saved under {}. \
                     Re-run with --variant --resume {run} --approve-long-run to continue.",
                    out_dir.display()
                );
                sidecar.stop().await;
                std::process::exit(3);
            }
        }
    }
    sidecar.stop().await;
    let md = write_variant_outputs(&out_dir, &run, &compare_to, &bs, &vbs, &orig);
    println!(
        "\nvariant run finished in {:.1} min. Pages: {}\n\n{md}",
        started.elapsed().as_secs_f64() / 60.0,
        out_dir.join("index.html").display()
    );
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
    variant: bool,
    compare_to: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        approve_long_run: false,
        system_pass: false,
        resume: None,
        models_dir: None,
        system_num_gpu: None,
        variant: false,
        compare_to: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--approve-long-run" => a.approve_long_run = true,
            "--system-pass" => a.system_pass = true,
            "--resume" => a.resume = it.next(),
            "--variant" => a.variant = true,
            "--compare-to" => a.compare_to = it.next(),
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
    if args.variant {
        assert!(
            !args.system_pass,
            "--variant and --system-pass are separate runs"
        );
        run_variant(args, home, root).await;
        return;
    }
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
        sidecar_base = start_sidecar(&mut sidecar, &home, &root, args.models_dir.clone()).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(s: &str) -> HashSet<String> {
        word_set(s)
    }

    #[test]
    fn placeholders_are_found_and_cut_out() {
        let (spans, rest) = split_placeholders("Dear [Name],\nSee [1] and {date} <Your Name>.\n");
        assert_eq!(spans, vec!["[Name]", "{date}", "<Your Name>"]);
        assert!(rest.contains("[1]"));
        assert!(!rest.contains("Name"));
    }

    #[test]
    fn opening_and_closing_chatter() {
        assert!(opening_chatter("Here is a polished draft:\n\nDear X,").is_some());
        assert!(opening_chatter("Sure, here you go.\nDear X,").is_some());
        assert!(opening_chatter("Dear Returns Department,\nSurely this is fine.").is_none());
        assert!(closing_chatter(
            "Dear X,\n\nBody.\n\nTomas\n\nLet me know if you'd like me to adjust the tone."
        )
        .is_some());
        assert!(
            closing_chatter("Dear X,\n\nPlease let me know if you need anything.\n\nTomas")
                .is_none()
        );
    }

    #[test]
    fn coverage_uses_word_starts_and_groups() {
        let hay = norm("The left hinge cracked in the 2nd week. Ticket CB-77804 only.");
        assert!(point_covered(
            &hay,
            &[&["hinge"], &["second week", "2nd week"]]
        ));
        assert!(!point_covered(&hay, &[&["hinge"], &["refund"]]));
        // "4" must start a word: it does not match inside cb-77804.
        assert!(!has_term(&hay, "4 ticket"));
        assert!(has_term(&hay, "cb-77804"));
    }

    #[test]
    fn unsupported_specifics_flags_invented_and_passes_supplied() {
        let a = allowed(
            "laptop serial VX14-0042718 bought on 3 August 2026 order BW-558201. \
             Tomas Brandvold, 418 Larch Street, Duluth, MN 55802. Returns department. five weeks",
        );
        let draft = "Tomas Brandvold\n418 Larch Street, Duluth, MN 55802\n\n13 November 2026\n\n\
                     Dear Returns Department,\n\n\
                     I bought the laptop (serial VX14-0042718) on 3 August 2026, order BW-558201, \
                     and called Mr. Harlow in Seattle on 12 September. It failed after 33 percent of \
                     the warranty, within three weeks.\n\nSincerely,\nTomas Brandvold";
        let u = unsupported_specifics(draft, &a);
        for expected in [
            "13",
            "November",
            "Harlow",
            "Seattle",
            "12",
            "September",
            "33",
            "three",
        ] {
            assert!(
                u.iter().any(|x| x == expected),
                "{expected} missing from {u:?}"
            );
        }
        for fine in [
            "VX14-0042718",
            "BW-558201",
            "3",
            "August",
            "Tomas",
            "Brandvold",
            "Duluth",
            "Returns",
            "Department",
            "Sincerely",
            "I",
        ] {
            assert!(
                !u.iter().any(|x| x == fine),
                "{fine} wrongly flagged in {u:?}"
            );
        }
    }

    #[test]
    fn changed_identifier_digit_is_flagged() {
        let a = allowed("serial VX14-0042718");
        let u = unsupported_specifics("Serial VX14-0042719 here.", &a);
        assert_eq!(u, vec!["VX14-0042719"]);
    }

    #[test]
    fn subject_line_and_list_markers_are_not_names() {
        let a = allowed("complaint about the router");
        let u = unsupported_specifics(
            "Subject: Formal Complaint Regarding Disruptions\n1. First point\n",
            &a,
        );
        assert!(u.is_empty(), "{u:?}");
    }

    #[test]
    fn variant_prompt_renders_without_leftover_tokens() {
        let bs = briefs();
        for (b, vb) in bs.iter().zip(variant_briefs().iter()) {
            let p = variant_prompt(b, vb);
            assert!(p.contains(vb.sender.name) && p.contains(vb.sender.address));
            assert!(p.contains("no opening or closing remarks"));
            assert!(!p.contains("polished draft"));
            assert_eq!(vb.id, b.id);
        }
    }

    #[test]
    fn every_variant_brief_has_points_with_keys() {
        for vb in variant_briefs() {
            assert!(vb.points.len() >= 5);
            for p in &vb.points {
                assert!(!p.keys.is_empty(), "{}", p.text);
            }
        }
    }
}

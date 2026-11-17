// src-tauri/examples/calibration_harness.rs
//
// DEV-ONLY. Not part of the shipped path: nothing in the app calls this, it is
// never built into a release artifact, and it is run by hand on a developer
// machine. items.id=899 stage 2 (decisions.id=950, decisions.id=942 point 12 and
// addendum 3 a to d). This is the harness CORE: generation and deterministic
// scoring. The rubric judge, the quick_response measures, read.html and the cell
// writer arrive in stage 3.
//
// What it does: for each shipped writing document type it takes synthetic briefs
// (examples/calibration_data/briefs.json, three per type), renders the prompt with
// the SHIPPED draft block (model_block_spec("cb-12"): parse and expand through the
// public registry, so the template, the latitude sentence, the length target and
// the drafting parameters are whatever the product would use, not an example-local
// prompt), generates a draft with each bundled local model on QR's own sidecar,
// and scores every draft with the shipped conductor::draft_check module. It saves
// every draft and writes a per-(type, model) summary.
//
// Rules this file follows:
//   - Synthetic data only. Every brief, name, address and organisation is invented.
//     No persona, database or key is read.
//   - Local only. The one network peer is QR's own Ollama sidecar on 127.0.0.1.
//     No hosted model, no hosted call, no key (items.id=788 is out of scope).
//   - No model or provider id is hard-coded in shipped code. The model tags below
//     are this example's run list, as in focus_calibration.rs (the bundled models).
//   - persist_result is never called, so scores.db is never created.
//   - Outputs go under $HOME/.cache/qr-899-calibration/<run>/, never /tmp.
//
// What the allowed text is: exactly what the prompt gave the model (the rendered
// brief, the sender details the type declares, the context fields the type
// declares), so "unsupported" means "not in the prompt". Latitude comes from the
// document type's own setting (registry), and decide(report, latitude, 0) says what
// the product would do with the FIRST draft. The harness does not regenerate: it
// reports how often a strict draft WOULD be regenerated.
//
// What the checks cannot catch is the draft_check module's list (soft embellishment,
// invented causes, a reversed fact that keeps the keywords). Nothing here scores
// quality; a human reads the borderline drafts (stage 3).
//
// Usage (from src-tauri/, debug build only):
//   cargo run --example calibration_harness -- --dry-run              # no Ollama
//   cargo run --example calibration_harness                            # full run
//   cargo run --example calibration_harness -- --samples 1 --per-type 1
//   cargo run --example calibration_harness -- --types rma_letter,cover_letter
//   cargo run --example calibration_harness -- --resume <run>
//   cargo run --example calibration_harness -- --summarize <run>      # rescore files

#[path = "support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use quietrabbit_lib::conductor::draft_check::{
    check, decide, CheckInput, CheckReport, Point, Verdict,
};
use quietrabbit_lib::conductor::model_block::brief::StructuredBrief;
use quietrabbit_lib::conductor::model_block::document_type::{
    registry, DocumentType, Latitude, SenderField,
};
use quietrabbit_lib::conductor::model_block::{model_block_spec, ExpandInput};
use quietrabbit_lib::conductor::tokens::{ActionDefinition, ExternalAccess};
use quietrabbit_lib::ollama_sidecar::OllamaSidecar;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use support::{
    generate, render_template, slug, start_sidecar, strip_think, warm_up, word_count, write,
    MODELS, REQUEST_TIMEOUT,
};

const CORPUS_JSON: &str = include_str!("calibration_data/briefs.json");
const DEFAULT_SAMPLES: u32 = 4;
const DEFAULT_PER_TYPE: usize = 3;
const LONG_RUN_LIMIT_SECS: f64 = 3600.0;
/// The block's draft step as the harness names it. Never reaches a run.
const ACTION_ID: &str = "calibration_draft";

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Corpus {
    briefs: Vec<BriefData>,
}

#[derive(Deserialize, Clone)]
struct BriefData {
    id: String,
    document_type: String,
    task: String,
    recipient: String,
    sender: SenderData,
    context: ContextData,
    points: Vec<PointData>,
}

#[derive(Deserialize, Clone)]
struct SenderData {
    name: String,
    address: String,
    date: String,
}

#[derive(Deserialize, Clone)]
struct ContextData {
    job_title: String,
    employer: String,
}

#[derive(Deserialize, Clone)]
struct PointData {
    text: String,
    keys: Vec<Vec<String>>,
}

fn corpus() -> Vec<BriefData> {
    serde_json::from_str::<Corpus>(CORPUS_JSON)
        .expect("calibration_data/briefs.json parses")
        .briefs
}

// ---------------------------------------------------------------------------
// Rendering through the shipped draft block
// ---------------------------------------------------------------------------

fn document_type(id: &str) -> &'static DocumentType {
    registry()
        .expect("registry")
        .document_type(id)
        .unwrap_or_else(|| panic!("document type {id} is not shipped"))
}

fn sender_text(t: &DocumentType, s: &SenderData) -> String {
    t.sender
        .iter()
        .map(|f| match f {
            SenderField::Name => format!("Name: {}", s.name),
            SenderField::Address => format!("Address: {}", s.address),
            SenderField::Date => format!("Date: {}", s.date),
            other => panic!("the corpus has no {other:?} sender detail"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn context_text(t: &DocumentType, c: &ContextData) -> String {
    t.context_fields
        .iter()
        .map(|f| match f.as_str() {
            "job_title" => format!("Job title: {}", c.job_title),
            "employer" => format!("Employer: {}", c.employer),
            other => panic!("the corpus has no context field {other}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The cb-12 row for a document type: the variables it needs, no others.
fn draft_row(t: &DocumentType) -> Value {
    let mut declared = vec!["content_brief"];
    if !t.sender.is_empty() {
        declared.push("sender_details");
    }
    if !t.context_fields.is_empty() {
        declared.push("writer_context");
    }
    json!({
        "common": {
            "display_name": "Calibration draft",
            "output_var": "draft_output",
            "need": {"task_type": "creative_writing", "difficulty": "low", "output_size": "medium"},
            "declared_inputs": declared,
            "range": {
                "quality_floor": {"min": "basic", "max": "standard"},
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": t.id,
            "brief_var": "content_brief",
            "sender_var": if t.sender.is_empty() { Value::Null } else { json!("sender_details") },
            "context_var": if t.context_fields.is_empty() { Value::Null } else { json!("writer_context") }
        }
    })
}

/// The ordinary step the shipped draft block expands into for a type, with the
/// type's shipped default settings (what LOAD uses).
fn expand_draft(t: &DocumentType) -> ActionDefinition {
    let spec = model_block_spec("cb-12").expect("cb-12 is registered");
    let parsed = (spec.parse)(&draft_row(t), false)
        .unwrap_or_else(|e| panic!("cb-12 row for {} does not parse: {e}", t.id));
    (spec.expand)(
        &parsed,
        &ExpandInput {
            action_id: ACTION_ID,
            focus_ceiling: ExternalAccess::LocalOnly,
            settings: None,
            external_allowed: false,
            document_type: None,
            envelope_override: None,
        },
    )
    .unwrap_or_else(|e| panic!("cb-12 for {} does not expand: {e}", t.id))
}

/// Everything one brief contributes: the prompt, and the text a draft may draw
/// its specifics from (the rendered inputs, nothing else).
struct Rendered {
    prompt: String,
    allowed_text: String,
}

fn render_brief(step: &ActionDefinition, t: &DocumentType, b: &BriefData) -> Rendered {
    let reg = registry().expect("registry");
    let brief = StructuredBrief {
        task: b.task.clone(),
        recipient: Some(b.recipient.clone()),
        points: b.points.iter().map(|p| p.text.clone()).collect(),
    };
    brief.validate().unwrap_or_else(|e| panic!("{}: {e}", b.id));
    let content_brief = brief.render(&reg.phrases.brief);
    let sender = sender_text(t, &b.sender);
    let context = context_text(t, &b.context);
    let mut tokens: BTreeMap<&str, String> = BTreeMap::new();
    tokens.insert("content_brief", content_brief.clone());
    let mut allowed = content_brief;
    if !sender.is_empty() {
        tokens.insert("sender_details", sender.clone());
        allowed.push('\n');
        allowed.push_str(&sender);
    }
    if !context.is_empty() {
        tokens.insert("writer_context", context.clone());
        allowed.push('\n');
        allowed.push_str(&context);
    }
    Rendered {
        prompt: render_template(&step.prompt_template, &tokens),
        allowed_text: allowed,
    }
}

/// Executor defaults, then the block's own parameters, then the num_ctx the
/// executor's get_context_window() table gives each bundled model.
fn options_for(step: &ActionDefinition, model: &str) -> Value {
    let mut o = json!({"temperature": 0.5, "top_p": 0.90, "num_predict": 1024});
    for (k, v) in &step.options_override {
        o[k.as_str()] = v.clone();
    }
    o["num_ctx"] = json!(match model {
        "llama3.2:3b" => 4096,
        "llama3.1:8b" | "qwen2.5:7b" => 8192,
        _ => 2048,
    });
    o
}

// ---------------------------------------------------------------------------
// Scoring one draft
// ---------------------------------------------------------------------------

/// Runs the shipped check on one draft of one brief.
fn check_draft(b: &BriefData, allowed_text: &str, text: &str) -> CheckReport {
    let alts: Vec<Vec<Vec<&str>>> = b
        .points
        .iter()
        .map(|p| {
            p.keys
                .iter()
                .map(|g| g.iter().map(String::as_str).collect())
                .collect()
        })
        .collect();
    let groups: Vec<Vec<&[&str]>> = alts
        .iter()
        .map(|p| p.iter().map(Vec::as_slice).collect())
        .collect();
    let points: Vec<Point> = b
        .points
        .iter()
        .zip(&groups)
        .map(|(p, g)| Point {
            text: &p.text,
            keys: g.as_slice(),
        })
        .collect();
    check(
        text,
        &CheckInput {
            allowed_text,
            points: &points,
        },
    )
}

// ---------------------------------------------------------------------------
// Records and saved drafts
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    brief: String,
    document_type: String,
    model: String,
    sample: u32,
    words: usize,
    latency_ms: f64,
    load_ms: f64,
    output_tokens: u64,
    hit_cap: bool,
    error: Option<String>,
}

/// One generation, reusing a finished file when resuming.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    http: &reqwest::Client,
    base_url: &str,
    out_dir: &Path,
    b: &BriefData,
    model: &str,
    sample: u32,
    prompt: &str,
    options: &Value,
) -> Record {
    let stem = format!("{}-{sample}", slug(model));
    let md = out_dir.join(&b.id).join(format!("{stem}.md"));
    let meta = out_dir.join(&b.id).join(format!("{stem}.json"));
    if let (true, Ok(m)) = (md.exists(), fs::read_to_string(&meta)) {
        if let Ok(r) = serde_json::from_str::<Record>(&m) {
            println!("  {stem}: reused");
            return r;
        }
    }
    let (text, g, error) = match generate(http, base_url, model, prompt, options).await {
        Ok(g) => {
            let (visible, _) = strip_think(&g.text);
            (visible, Some(g), None)
        }
        Err(e) => (String::new(), None, Some(e)),
    };
    let record = Record {
        brief: b.id.clone(),
        document_type: b.document_type.clone(),
        model: model.to_owned(),
        sample,
        words: word_count(&text),
        latency_ms: g.as_ref().map_or(0.0, |g| g.latency_ms),
        load_ms: g.as_ref().map_or(0.0, |g| g.load_ms),
        output_tokens: g.as_ref().map_or(0, |g| g.output_tokens),
        hit_cap: g.as_ref().is_some_and(|g| g.hit_cap),
        error,
    };
    write(&md, &text);
    write(
        &meta,
        &serde_json::to_string_pretty(&record).expect("serialize record"),
    );
    println!(
        "  {stem}: {} words, {:.1}s{}{}",
        record.words,
        record.latency_ms / 1000.0,
        if record.hit_cap {
            ", hit token cap"
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

/// A saved draft with its record and its check.
struct Draft {
    rec: Record,
    report: CheckReport,
    verdict: Verdict,
}

fn load_drafts(out_dir: &Path, briefs: &[BriefData], models: &[String]) -> Vec<Draft> {
    let mut out = Vec::new();
    for b in briefs {
        let t = document_type(&b.document_type);
        let step = expand_draft(t);
        let rendered = render_brief(&step, t, b);
        let latitude = t.settings.latitude;
        for m in models {
            let mut n = 1;
            loop {
                let stem = format!("{}-{n}", slug(m));
                let base = out_dir.join(&b.id);
                let (Ok(js), Ok(text)) = (
                    fs::read_to_string(base.join(format!("{stem}.json"))),
                    fs::read_to_string(base.join(format!("{stem}.md"))),
                ) else {
                    break;
                };
                n += 1;
                let Ok(rec) = serde_json::from_str::<Record>(&js) else {
                    continue;
                };
                let report = check_draft(b, &rendered.allowed_text, &text);
                let verdict = decide(&report, latitude, 0);
                out.push(Draft {
                    rec,
                    report,
                    verdict,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Summary per (document type, model)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug, PartialEq)]
struct CellStats {
    document_type: String,
    latitude: String,
    model: String,
    drafts: usize,
    errors: usize,
    briefs: usize,
    words_mean: f64,
    words_min: usize,
    words_max: usize,
    /// Standard deviation of the word count over its mean, in percent.
    words_cv_pct: f64,
    placeholder_drafts: usize,
    opening_chatter_drafts: usize,
    closing_chatter_drafts: usize,
    unsupported_drafts: usize,
    unsupported_total: usize,
    /// Drafts with any finding (placeholder, chatter, unsupported specific).
    finding_drafts: usize,
    /// Covered points over listed points, percent.
    coverage_pct: f64,
    full_coverage_drafts: usize,
    verdict_accept: usize,
    verdict_regenerate: usize,
    verdict_mark: usize,
    verdict_skipped: usize,
    hit_cap_drafts: usize,
    latency_mean_s: f64,
}

fn latitude_str(l: Latitude) -> &'static str {
    l.as_str()
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn cell_stats(document_type: &str, model: &str, ds: &[&Draft]) -> CellStats {
    let ok: Vec<&&Draft> = ds.iter().filter(|d| d.rec.error.is_none()).collect();
    let words: Vec<f64> = ok.iter().map(|d| d.rec.words as f64).collect();
    let wm = mean(&words);
    let var = if words.len() > 1 {
        words.iter().map(|w| (w - wm).powi(2)).sum::<f64>() / words.len() as f64
    } else {
        0.0
    };
    let cov_total: usize = ok.iter().map(|d| d.report.points_total).sum();
    let cov_done: usize = ok.iter().map(|d| d.report.points_covered).sum();
    let count = |f: &dyn Fn(&Draft) -> bool| ok.iter().filter(|d| f(d)).count();
    let verdict = |v: Verdict| count(&|d| d.verdict == v);
    let briefs: std::collections::BTreeSet<&str> =
        ds.iter().map(|d| d.rec.brief.as_str()).collect();
    CellStats {
        document_type: document_type.to_owned(),
        latitude: latitude_str(document_type_latitude(document_type)).to_owned(),
        model: model.to_owned(),
        drafts: ds.len(),
        errors: ds.len() - ok.len(),
        briefs: briefs.len(),
        words_mean: wm,
        words_min: ok.iter().map(|d| d.rec.words).min().unwrap_or(0),
        words_max: ok.iter().map(|d| d.rec.words).max().unwrap_or(0),
        words_cv_pct: if wm > 0.0 {
            var.sqrt() / wm * 100.0
        } else {
            0.0
        },
        placeholder_drafts: count(&|d| !d.report.placeholders.is_empty()),
        opening_chatter_drafts: count(&|d| d.report.opening_chatter.is_some()),
        closing_chatter_drafts: count(&|d| d.report.closing_chatter.is_some()),
        unsupported_drafts: count(&|d| !d.report.unsupported.is_empty()),
        unsupported_total: ok.iter().map(|d| d.report.unsupported.len()).sum(),
        finding_drafts: count(&|d| d.report.has_findings()),
        coverage_pct: if cov_total == 0 {
            0.0
        } else {
            cov_done as f64 / cov_total as f64 * 100.0
        },
        full_coverage_drafts: count(&|d| d.report.points_covered == d.report.points_total),
        verdict_accept: verdict(Verdict::Accept),
        verdict_regenerate: verdict(Verdict::Regenerate),
        verdict_mark: verdict(Verdict::Mark),
        verdict_skipped: verdict(Verdict::Skipped),
        hit_cap_drafts: count(&|d| d.rec.hit_cap),
        latency_mean_s: mean(
            &ok.iter()
                .map(|d| d.rec.latency_ms / 1000.0)
                .collect::<Vec<_>>(),
        ),
    }
}

fn document_type_latitude(id: &str) -> Latitude {
    document_type(id).settings.latitude
}

fn all_cells(drafts: &[Draft], types: &[String], models: &[String]) -> Vec<CellStats> {
    let mut out = Vec::new();
    for t in types {
        for m in models {
            let ds: Vec<&Draft> = drafts
                .iter()
                .filter(|d| &d.rec.document_type == t && &d.rec.model == m)
                .collect();
            if !ds.is_empty() {
                out.push(cell_stats(t, m, &ds));
            }
        }
    }
    out
}

const NOTES: &[&str] = &[
    "Every measure is deterministic (conductor::draft_check). Nothing here is a quality score.",
    "A finding is a placeholder, opening or closing chatter, or an unsupported specific. Missing points are coverage, not a finding.",
    "Verdicts are decide(report, the type's latitude, attempt 0): what the product would do with the FIRST draft. The harness does not regenerate.",
    "The check cannot catch soft embellishment, invented causes, a reversed fact that keeps the keywords, tone or readability.",
    "Latency is steady state (each model is warmed up with the same options before its samples).",
];

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "-".to_owned()
    } else {
        format!("{:.0}%", n as f64 / d as f64 * 100.0)
    }
}

const HEADER: [&str; 14] = [
    "type",
    "latitude",
    "model",
    "drafts",
    "words (mean, min-max, cv)",
    "placeholder",
    "chatter open/close",
    "unsupported drafts (total)",
    "any finding",
    "coverage",
    "full coverage",
    "accept / regen / mark / skip",
    "hit cap",
    "latency s",
];

fn cell_row(c: &CellStats) -> Vec<String> {
    let d = c.drafts - c.errors;
    vec![
        c.document_type.clone(),
        c.latitude.clone(),
        c.model.clone(),
        if c.errors > 0 {
            format!("{} ({} errors)", c.drafts, c.errors)
        } else {
            c.drafts.to_string()
        },
        format!(
            "{:.0}, {}-{}, {:.0}%",
            c.words_mean, c.words_min, c.words_max, c.words_cv_pct
        ),
        pct(c.placeholder_drafts, d),
        format!(
            "{} / {}",
            pct(c.opening_chatter_drafts, d),
            pct(c.closing_chatter_drafts, d)
        ),
        format!("{} ({})", pct(c.unsupported_drafts, d), c.unsupported_total),
        pct(c.finding_drafts, d),
        format!("{:.0}%", c.coverage_pct),
        pct(c.full_coverage_drafts, d),
        format!(
            "{} / {} / {} / {}",
            c.verdict_accept, c.verdict_regenerate, c.verdict_mark, c.verdict_skipped
        ),
        c.hit_cap_drafts.to_string(),
        format!("{:.1}", c.latency_mean_s),
    ]
}

fn summary_md(cells: &[CellStats], run: &str) -> String {
    let mut s = format!("# Calibration harness summary: {run}\n\n");
    s.push_str(&format!("| {} |\n", HEADER.join(" | ")));
    s.push_str(&format!("|{}|\n", vec!["---"; HEADER.len()].join("|")));
    for c in cells {
        s.push_str(&format!("| {} |\n", cell_row(c).join(" | ")));
    }
    s.push('\n');
    for n in NOTES {
        s.push_str(&format!("- {n}\n"));
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn index_html(cells: &[CellStats], run: &str, briefs: &[BriefData]) -> String {
    let mut s = format!(
        "<!doctype html><meta charset=utf-8><title>{run}</title>\
         <style>body{{font-family:system-ui,sans-serif;margin:1.5rem}}\
         table{{border-collapse:collapse;font-size:.85rem}}\
         td,th{{border:1px solid #bbb;padding:.25rem .5rem;text-align:left}}\
         th{{background:#eee}}</style><h1>Calibration harness: {run}</h1><table><tr>",
        run = esc(run)
    );
    for h in HEADER {
        s.push_str(&format!("<th>{}</th>", esc(h)));
    }
    s.push_str("</tr>");
    for c in cells {
        s.push_str("<tr>");
        for v in cell_row(c) {
            s.push_str(&format!("<td>{}</td>", esc(&v)));
        }
        s.push_str("</tr>");
    }
    s.push_str("</table><ul>");
    for n in NOTES {
        s.push_str(&format!("<li>{}</li>", esc(n)));
    }
    s.push_str("</ul><h2>Briefs</h2><ul>");
    for b in briefs {
        s.push_str(&format!(
            "<li>{} ({}): <a href=\"{}/prompt.txt\">prompt</a></li>",
            esc(&b.id),
            esc(&b.document_type),
            esc(&b.id)
        ));
    }
    s.push_str("</ul>");
    s
}

fn write_outputs(out_dir: &Path, run: &str, briefs: &[BriefData], models: &[String]) -> String {
    let drafts = load_drafts(out_dir, briefs, models);
    let mut types: Vec<String> = Vec::new();
    for b in briefs {
        if !types.contains(&b.document_type) {
            types.push(b.document_type.clone());
        }
    }
    let cells = all_cells(&drafts, &types, models);
    write(
        &out_dir.join("summary.json"),
        &serde_json::to_string_pretty(&cells).expect("serialize cells"),
    );
    let md = summary_md(&cells, run);
    write(&out_dir.join("summary.md"), &md);
    write(
        &out_dir.join("index.html"),
        &index_html(&cells, run, briefs),
    );
    md
}

// ---------------------------------------------------------------------------
// Selection and arguments
// ---------------------------------------------------------------------------

struct Args {
    samples: u32,
    per_type: usize,
    types: Option<Vec<String>>,
    models: Option<Vec<String>>,
    resume: Option<String>,
    summarize: Option<String>,
    dry_run: bool,
    approve_long_run: bool,
    models_dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        samples: DEFAULT_SAMPLES,
        per_type: DEFAULT_PER_TYPE,
        types: None,
        models: None,
        resume: None,
        summarize: None,
        dry_run: false,
        approve_long_run: false,
        models_dir: None,
    };
    let list = |v: Option<String>| -> Vec<String> {
        v.expect("a comma-separated list")
            .split(',')
            .map(str::to_owned)
            .collect()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--samples" => a.samples = it.next().and_then(|v| v.parse().ok()).expect("--samples N"),
            "--per-type" => {
                a.per_type = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--per-type N")
            }
            "--types" => a.types = Some(list(it.next())),
            "--models" => a.models = Some(list(it.next())),
            "--resume" => a.resume = it.next(),
            "--summarize" => a.summarize = it.next(),
            "--dry-run" => a.dry_run = true,
            "--approve-long-run" => a.approve_long_run = true,
            "--models-dir" => a.models_dir = it.next().map(PathBuf::from),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// The briefs a run uses: the chosen types (default every shipped type in the
/// corpus), the first `per_type` briefs of each, in corpus order.
fn select(all: &[BriefData], types: &Option<Vec<String>>, per_type: usize) -> Vec<BriefData> {
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for b in all {
        if let Some(ts) = types {
            if !ts.contains(&b.document_type) {
                continue;
            }
        }
        let n = count.entry(b.document_type.as_str()).or_insert(0);
        if *n < per_type {
            *n += 1;
            out.push(b.clone());
        }
    }
    out
}

fn models_of(args: &Args) -> Vec<String> {
    args.models
        .clone()
        .unwrap_or_else(|| MODELS.iter().map(|m| (*m).to_owned()).collect())
}

#[tokio::main]
async fn main() {
    if !cfg!(debug_assertions) {
        eprintln!("calibration_harness is dev-only and runs in a debug build");
        std::process::exit(2);
    }
    let args = parse_args();
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let root = home.join(".cache/qr-899-calibration");
    let all = corpus();
    let briefs = select(&all, &args.types, args.per_type);
    let models = models_of(&args);
    assert!(!briefs.is_empty(), "no brief matches --types");

    if let Some(run) = &args.summarize {
        let md = write_outputs(&root.join(run), run, &briefs, &models);
        println!("{md}");
        return;
    }

    let run = args.resume.clone().unwrap_or_else(|| {
        chrono::Local::now()
            .format(if args.dry_run {
                "dry-%Y%m%d-%H%M%S"
            } else {
                "run-%Y%m%d-%H%M%S"
            })
            .to_string()
    });
    let out_dir = root.join(&run);
    fs::create_dir_all(&out_dir).expect("create run dir");
    let per_brief = models.len() as u32 * args.samples;
    println!(
        "{} briefs x {} models x {} samples = {} drafts; output {}",
        briefs.len(),
        models.len(),
        args.samples,
        briefs.len() as u32 * per_brief,
        out_dir.display()
    );

    // Render every prompt first: a broken template or corpus fails before any run.
    let mut prompts: Vec<(BriefData, ActionDefinition, Rendered)> = Vec::new();
    for b in &briefs {
        let t = document_type(&b.document_type);
        let step = expand_draft(t);
        let r = render_brief(&step, t, b);
        write(&out_dir.join(&b.id).join("prompt.txt"), &r.prompt);
        prompts.push((b.clone(), step, r));
    }
    if args.dry_run {
        println!(
            "dry run: {} prompts rendered under {}; nothing generated",
            prompts.len(),
            out_dir.display()
        );
        return;
    }

    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("http client");
    let mut sidecar = OllamaSidecar::new();
    let base = start_sidecar(&mut sidecar, &home, &root, args.models_dir.clone()).await;

    let started = Instant::now();
    for (bi, (b, step, r)) in prompts.iter().enumerate() {
        let brief_start = Instant::now();
        println!("\n== {} ({}/{})", b.id, bi + 1, prompts.len());
        for m in &models {
            let opts = options_for(step, m);
            let finished = (1..=args.samples).all(|n| {
                out_dir
                    .join(&b.id)
                    .join(format!("{}-{n}.json", slug(m)))
                    .exists()
            });
            if !finished {
                warm_up(&http, &base, m, &opts).await;
            }
            for n in 1..=args.samples {
                run_one(&http, &base, &out_dir, b, m, n, &r.prompt, &opts).await;
            }
        }
        write_outputs(&out_dir, &run, &briefs, &models);
        if bi == 0 {
            let first = brief_start.elapsed().as_secs_f64();
            let projected = first * prompts.len() as f64;
            println!(
                "\nTIMING after brief 1: {:.1} min ({} drafts). Projected full run if the other briefs cost \
                 the same: {:.0} min ({:.0} min still to go). Long essays and stories cost more, so treat this as a floor.",
                first / 60.0,
                per_brief,
                projected / 60.0,
                first * (prompts.len() - 1) as f64 / 60.0
            );
            if projected > LONG_RUN_LIMIT_SECS && !args.approve_long_run {
                println!(
                    "STOP: the projected run is over an hour. Brief 1 is saved under {}. Re-run with \
                     --resume {run} --approve-long-run to continue.",
                    out_dir.display()
                );
                sidecar.stop().await;
                std::process::exit(3);
            }
        }
    }
    sidecar.stop().await;
    let md = write_outputs(&out_dir, &run, &briefs, &models);
    println!(
        "\nfinished in {:.1} min. Pages: {}\n\n{md}",
        started.elapsed().as_secs_f64() / 60.0,
        out_dir.join("index.html").display()
    );
}

// ---------------------------------------------------------------------------
// Tests (run with: cargo test --example calibration_harness)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED_TYPES: [&str; 6] = [
        "rma_letter",
        "complaint_email",
        "personal_note",
        "cover_letter",
        "essay_section",
        "creative_story",
    ];

    #[test]
    fn the_corpus_has_three_unique_briefs_for_every_shipped_type() {
        let all = corpus();
        let mut ids = std::collections::HashSet::new();
        for b in &all {
            assert!(ids.insert(b.id.clone()), "duplicate id {}", b.id);
            assert!(b.id.starts_with(&b.document_type), "{}", b.id);
        }
        for t in SHIPPED_TYPES {
            assert_eq!(
                all.iter().filter(|b| b.document_type == t).count(),
                3,
                "{t}"
            );
        }
        assert_eq!(all.len(), 18);
        // The test list is the registry's list: a new shipped type forces a decision.
        assert_eq!(registry().unwrap().types.len(), SHIPPED_TYPES.len());
    }

    #[test]
    fn every_point_has_keys_and_is_covered_by_its_own_text() {
        for b in corpus() {
            assert!(b.points.len() >= 4, "{}", b.id);
            for p in &b.points {
                assert!(!p.keys.is_empty(), "{}: {}", b.id, p.text);
                assert!(p.keys.iter().all(|g| !g.is_empty()), "{}: {}", b.id, p.text);
            }
            // A faithful restatement of the points must cover every point, or the
            // keyword groups can never be met.
            let restated = b
                .points
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let r = check_draft(&b, "", &restated);
            assert!(
                r.missing_points.is_empty(),
                "{}: keys cannot match their own point: {:?}",
                b.id,
                r.missing_points
            );
        }
    }

    #[test]
    fn every_brief_renders_through_the_shipped_draft_block_without_leftovers() {
        for b in corpus() {
            let t = document_type(&b.document_type);
            let step = expand_draft(t);
            assert_eq!(step.task_type, "creative_writing");
            assert!(step.requirement.is_some(), "an expanded block carries it");
            let r = render_brief(&step, t, &b); // panics on a leftover token
            for p in &b.points {
                assert!(r.prompt.contains(&p.text), "{}: {}", b.id, p.text);
                assert!(r.allowed_text.contains(&p.text));
            }
            assert_eq!(r.prompt.contains(&b.sender.name), !t.sender.is_empty());
            assert_eq!(
                r.prompt.contains(&b.context.employer),
                !t.context_fields.is_empty()
            );
            // The latitude sentence of the type's own setting is in the prompt.
            let phrase = registry()
                .unwrap()
                .phrases
                .draft
                .latitude(t.settings.latitude);
            assert!(r.prompt.contains(phrase), "{}", b.id);
        }
    }

    #[test]
    fn a_faithful_draft_has_no_findings_and_an_invented_one_does() {
        let all = corpus();
        let b = all.iter().find(|b| b.id == "personal_note-3").unwrap();
        let t = document_type(&b.document_type);
        let r = render_brief(&expand_draft(t), t, b);
        let good = "Dear Aunt Berta,\n\nI am so sorry I missed your 70th birthday on 2 November. \
                    I was ill with flu that week. I will visit on the first weekend of December and \
                    I am sending a book of garden photographs. Love,\nTheo";
        let rep = check_draft(b, &r.allowed_text, good);
        assert!(!rep.has_findings(), "{rep:?}");
        assert!(rep.missing_points.is_empty(), "{:?}", rep.missing_points);
        let bad = "Here is a note:\n\nDear Aunt Berta, I missed your party in Oslo on [date].";
        let rep = check_draft(b, &r.allowed_text, bad);
        assert!(rep.has_findings());
        assert!(rep.opening_chatter.is_some() && !rep.placeholders.is_empty());
        assert!(rep.unsupported.iter().any(|u| u == "Oslo"));
    }

    #[test]
    fn options_follow_the_block_and_the_executor_table() {
        for b in corpus().iter().take(6) {
            let t = document_type(&b.document_type);
            let step = expand_draft(t);
            let o = options_for(&step, "llama3.2:3b");
            assert_eq!(o["num_ctx"], 4096);
            assert!(o["temperature"].as_f64().is_some());
            assert!(o["num_predict"].as_u64().unwrap() > 0);
        }
    }

    #[test]
    fn selection_takes_the_first_n_briefs_of_each_chosen_type() {
        let all = corpus();
        assert_eq!(select(&all, &None, 3).len(), 18);
        assert_eq!(select(&all, &None, 1).len(), 6);
        let two = select(&all, &Some(vec!["rma_letter".to_owned()]), 2);
        assert_eq!(
            two.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            ["rma_letter-1", "rma_letter-2"]
        );
    }

    fn draft(brief: &str, words: usize, report: CheckReport, v: Verdict) -> Draft {
        Draft {
            rec: Record {
                brief: brief.to_owned(),
                document_type: "rma_letter".to_owned(),
                model: "m".to_owned(),
                sample: 1,
                words,
                latency_ms: 2000.0,
                load_ms: 0.0,
                output_tokens: 10,
                hit_cap: false,
                error: None,
            },
            report,
            verdict: v,
        }
    }

    #[test]
    fn cell_stats_count_findings_coverage_and_verdicts() {
        let clean = CheckReport {
            points_total: 4,
            points_covered: 4,
            ..CheckReport::default()
        };
        let dirty = CheckReport {
            points_total: 4,
            points_covered: 2,
            placeholders: vec!["[x]".to_owned()],
            unsupported: vec!["Leeds".to_owned(), "9".to_owned()],
            ..CheckReport::default()
        };
        let ds = [
            draft("a", 100, clean.clone(), Verdict::Accept),
            draft("a", 120, dirty.clone(), Verdict::Regenerate),
            draft("b", 80, dirty, Verdict::Mark),
            draft("b", 100, clean, Verdict::Accept),
        ];
        let refs: Vec<&Draft> = ds.iter().collect();
        let c = cell_stats("rma_letter", "m", &refs);
        assert_eq!((c.drafts, c.errors, c.briefs), (4, 0, 2));
        assert_eq!(c.finding_drafts, 2);
        assert_eq!((c.placeholder_drafts, c.unsupported_total), (2, 4));
        assert_eq!(c.full_coverage_drafts, 2);
        assert!((c.coverage_pct - 75.0).abs() < 1e-9);
        assert_eq!(
            (
                c.verdict_accept,
                c.verdict_regenerate,
                c.verdict_mark,
                c.verdict_skipped
            ),
            (2, 1, 1, 0)
        );
        assert_eq!((c.words_min, c.words_max), (80, 120));
        assert_eq!(c.latitude, "strict");
        assert!((c.latency_mean_s - 2.0).abs() < 1e-9);
    }

    #[test]
    fn an_errored_draft_is_counted_but_not_scored() {
        let mut d = draft("a", 0, CheckReport::default(), Verdict::Accept);
        d.rec.error = Some("boom".to_owned());
        let c = cell_stats("rma_letter", "m", &[&d]);
        assert_eq!((c.drafts, c.errors, c.finding_drafts), (1, 1, 0));
        assert_eq!(c.verdict_accept, 0);
    }

    #[test]
    fn the_summary_and_index_render_without_panicking() {
        let c = cell_stats(
            "cover_letter",
            "m",
            &[&draft("a", 100, CheckReport::default(), Verdict::Mark)],
        );
        let md = summary_md(std::slice::from_ref(&c), "run-x");
        assert!(md.contains("cover_letter") && md.contains("framed"));
        let html = index_html(&[c], "run-<x>", &corpus());
        assert!(html.contains("run-&lt;x&gt;") && html.contains("rma_letter-1/prompt.txt"));
    }

    #[test]
    fn this_file_names_no_hosted_provider_and_no_remote_url() {
        let s = include_str!("calibration_harness.rs");
        let body = s.split("#[cfg(test)]").next().unwrap().to_lowercase();
        for banned in [
            "https://",
            "api_key",
            "openai",
            "anthropic",
            "authorization",
        ] {
            assert!(!body.contains(banned), "names '{banned}'");
        }
    }
}

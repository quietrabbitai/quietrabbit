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

mod harness {
    #[path = "../harness/judge.rs"]
    pub mod judge;
    #[path = "../harness/reading.rs"]
    pub mod reading;
    #[path = "../harness/voice.rs"]
    pub mod voice;
}
use harness::{judge, reading, voice};

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

/// Runs the shipped check on one draft against a list of points.
fn check_with(points: &[PointData], allowed_text: &str, text: &str) -> CheckReport {
    let alts: Vec<Vec<Vec<&str>>> = points
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
    let pts: Vec<Point> = points
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
            points: &pts,
            // items.id=903: the harness keeps the pre-903 scan, so its measurements stay
            // comparable with items.id=893/894.
            citations: quietrabbit_lib::conductor::draft_check::CitationMarkers::None,
        },
    )
}

/// Runs the shipped check on one draft of one brief.
fn check_draft(b: &BriefData, allowed_text: &str, text: &str) -> CheckReport {
    check_with(&b.points, allowed_text, text)
}

// ---------------------------------------------------------------------------
// Records and saved drafts
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct Record {
    pub brief: String,
    pub document_type: String,
    pub model: String,
    pub sample: u32,
    pub words: usize,
    pub latency_ms: f64,
    pub load_ms: f64,
    pub output_tokens: u64,
    pub hit_cap: bool,
    pub error: Option<String>,
}

/// Where one prompt's drafts live and what group they belong to.
struct Target {
    /// The brief or voice case id.
    id: String,
    /// The folder under the run directory: the brief id, or voice/<case id>.
    dir: String,
    /// The document type (writing) or voice_apply / voice_refine.
    group: String,
}

/// One generation, reusing a finished file when resuming.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    http: &reqwest::Client,
    base_url: &str,
    out_dir: &Path,
    t: &Target,
    model: &str,
    sample: u32,
    prompt: &str,
    options: &Value,
) -> Record {
    let stem = format!("{}-{sample}", slug(model));
    let md = out_dir.join(&t.dir).join(format!("{stem}.md"));
    let meta = out_dir.join(&t.dir).join(format!("{stem}.json"));
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
        brief: t.id.clone(),
        document_type: t.group.clone(),
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Writing,
    VoiceApply,
    VoiceRefine,
}

/// A saved draft with its record and its check.
pub struct Draft {
    /// Path under the run directory without the extension: <brief>/<stem> or
    /// voice/<case>/<stem>. Stable, and the key of every judge record and read.
    pub id: String,
    pub kind: Kind,
    /// The document type (writing) or voice_apply / voice_refine.
    pub group: String,
    /// The brief or voice case id.
    pub case: String,
    pub rec: Record,
    pub text: String,
    /// The prompt the writer was given.
    pub instructions: String,
    pub report: CheckReport,
    pub verdict: Verdict,
}

impl Draft {
    pub fn errored(&self) -> bool {
        self.rec.error.is_some() || self.text.trim().is_empty()
    }
}

/// Reads the saved samples of one prompt for each model: sample 1, 2, ... until a
/// file is missing.
fn saved_samples(out_dir: &Path, dir: &str, models: &[String]) -> Vec<(String, Record, String)> {
    let mut out = Vec::new();
    for m in models {
        let mut n = 1;
        loop {
            let stem = format!("{}-{n}", slug(m));
            let base = out_dir.join(dir);
            let (Ok(js), Ok(text)) = (
                fs::read_to_string(base.join(format!("{stem}.json"))),
                fs::read_to_string(base.join(format!("{stem}.md"))),
            ) else {
                break;
            };
            n += 1;
            if let Ok(rec) = serde_json::from_str::<Record>(&js) {
                out.push((format!("{dir}/{stem}"), rec, text));
            }
        }
    }
    out
}

fn load_drafts(out_dir: &Path, briefs: &[BriefData], models: &[String]) -> Vec<Draft> {
    let mut out = Vec::new();
    for b in briefs {
        let t = document_type(&b.document_type);
        let step = expand_draft(t);
        let rendered = render_brief(&step, t, b);
        let latitude = t.settings.latitude;
        for (id, rec, text) in saved_samples(out_dir, &b.id, models) {
            let report = check_draft(b, &rendered.allowed_text, &text);
            let verdict = decide(&report, latitude, 0);
            out.push(Draft {
                id,
                kind: Kind::Writing,
                group: b.document_type.clone(),
                case: b.id.clone(),
                rec,
                text,
                instructions: rendered.prompt.clone(),
                report,
                verdict,
            });
        }
    }
    out
}

/// The voice drafts. A rewrite or a piece of guidance must add nothing that was
/// not supplied, so the strict policy applies to both.
fn load_voice_drafts(out_dir: &Path, models: &[String]) -> Vec<Draft> {
    let c = voice::corpus();
    let mut out = Vec::new();
    for (case_id, mode, _t, _step, r) in voice::all_cases(&c) {
        let points: &[PointData] = match mode {
            voice::Mode::Apply => {
                &c.apply
                    .iter()
                    .find(|a| a.id == case_id)
                    .expect("apply case")
                    .points
            }
            voice::Mode::Refine => &[],
        };
        for (id, rec, text) in saved_samples(out_dir, &format!("voice/{case_id}"), models) {
            let report = check_with(points, &r.allowed_text, &text);
            let verdict = decide(&report, Latitude::Strict, 0);
            out.push(Draft {
                id,
                kind: match mode {
                    voice::Mode::Apply => Kind::VoiceApply,
                    voice::Mode::Refine => Kind::VoiceRefine,
                },
                group: mode.group().to_owned(),
                case: case_id.clone(),
                rec,
                text,
                instructions: r.prompt.clone(),
                report,
                verdict,
            });
        }
    }
    out
}

fn load_all(
    out_dir: &Path,
    briefs: &[BriefData],
    models: &[String],
    with_voice: bool,
) -> Vec<Draft> {
    let mut d = load_drafts(out_dir, briefs, models);
    if with_voice {
        d.extend(load_voice_drafts(out_dir, models));
    }
    d
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
        latitude: registry()
            .ok()
            .and_then(|r| r.document_type(document_type))
            .map_or("-", |t| latitude_str(t.settings.latitude))
            .to_owned(),
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

fn write_outputs(
    out_dir: &Path,
    run: &str,
    briefs: &[BriefData],
    models: &[String],
    with_voice: bool,
) -> String {
    let drafts = load_all(out_dir, briefs, models, with_voice);
    let mut groups: Vec<String> = Vec::new();
    for b in briefs {
        if !groups.contains(&b.document_type) {
            groups.push(b.document_type.clone());
        }
    }
    if with_voice {
        groups.push(voice::GROUP_APPLY.to_owned());
        groups.push(voice::GROUP_REFINE.to_owned());
    }
    let cells = all_cells(&drafts, &groups, models);
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
    no_voice: bool,
    voice_only: bool,
    judge: bool,
    judge_tags: Option<Vec<String>>,
    judge_url: Option<String>,
    read_page: Option<String>,
    apply_ratings: Option<(String, PathBuf)>,
    emit_cells: Option<String>,
    date: Option<String>,
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
        no_voice: false,
        voice_only: false,
        judge: false,
        judge_tags: None,
        judge_url: None,
        read_page: None,
        apply_ratings: None,
        emit_cells: None,
        date: None,
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
            "--no-voice" => a.no_voice = true,
            "--voice-only" => a.voice_only = true,
            "--judge" => a.judge = true,
            "--judge-tags" => a.judge_tags = Some(list(it.next())),
            // A local Ollama other than QR's sidecar (for a hand-run experiment). It
            // must be on this machine: any other host is refused.
            "--judge-url" => a.judge_url = it.next(),
            "--read-page" => a.read_page = it.next(),
            "--apply-ratings" => {
                let run = it.next().expect("--apply-ratings <run> <file>");
                let file = it.next().expect("--apply-ratings <run> <file>");
                a.apply_ratings = Some((run, PathBuf::from(file)));
            }
            "--emit-cells" => a.emit_cells = it.next(),
            "--date" => a.date = it.next(),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(
        !(a.no_voice && a.voice_only),
        "--no-voice and --voice-only are exclusive"
    );
    if let Some(u) = &a.judge_url {
        assert!(
            local_url(u),
            "--judge-url must be http://127.0.0.1:<port> or http://localhost:<port>"
        );
    }
    a
}

/// Only this machine: the judge sees draft text, so it never goes further. The
/// URL is parsed, not prefix-matched: scheme http, host exactly 127.0.0.1,
/// localhost or ::1 (the url crate prints an IPv6 host in brackets), and no user
/// name or password, so `http://localhost:80@evil.com/` (host evil.com) is refused.
fn local_url(u: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(u) else {
        return false;
    };
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
        )
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

/// Judges: by default the bundled models (each grades only drafts it did not write).
fn judge_tags_of(args: &Args) -> Vec<String> {
    args.judge_tags
        .clone()
        .unwrap_or_else(|| MODELS.iter().map(|m| (*m).to_owned()).collect())
}

/// Builds read.html and read_manifest.json from the saved drafts and verdicts.
fn build_read_page(
    out_dir: &Path,
    run: &str,
    briefs: &[BriefData],
    models: &[String],
    with_voice: bool,
    tags: &[String],
) -> reading::Manifest {
    let cfg = reading::config();
    let drafts = load_all(out_dir, briefs, models, with_voice);
    let verdicts = judge::load_verdicts(out_dir, &drafts, tags);
    let manifest = reading::select(run, &drafts, &verdicts, tags, &cfg);
    write(
        &out_dir.join("read_manifest.json"),
        &serde_json::to_string_pretty(&manifest).expect("serialize manifest"),
    );
    write(
        &out_dir.join("read.html"),
        &reading::read_html(&manifest, &drafts, &verdicts),
    );
    println!(
        "read page: {} drafts to read (limit {}), {} cell(s) not reached. {}",
        manifest.entries.len(),
        manifest.max_total,
        manifest.unreached.len(),
        out_dir.join("read.html").display()
    );
    manifest
}

fn apply_ratings(
    root: &Path,
    run: &str,
    file: &Path,
    briefs: &[BriefData],
    models: &[String],
    with_voice: bool,
    tags: &[String],
) {
    let cfg = reading::config();
    let out_dir = root.join(run);
    let manifest: reading::Manifest = serde_json::from_str(
        &fs::read_to_string(out_dir.join("read_manifest.json"))
            .expect("read_manifest.json (run --read-page first)"),
    )
    .expect("manifest parses");
    let text = fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let reads = reading::parse_ratings(&text, &manifest, &cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let (cells, unmeasured) = reading::build_cells(&reads, &manifest, &cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let drafts = load_all(&out_dir, briefs, models, with_voice);
    let verdicts = judge::load_verdicts(&out_dir, &drafts, tags);
    let agr = reading::agreement(&reads, &verdicts);
    write(&out_dir.join("ratings.json"), &text);
    write(
        &out_dir.join("cells.json"),
        &serde_json::to_string_pretty(
            &json!({"run": run, "cells": cells, "unmeasured": unmeasured}),
        )
        .expect("serialize cells"),
    );
    let md = reading::agreement_md(&agr, reads.len());
    write(&out_dir.join("agreement.md"), &md);
    println!(
        "{} reads: {} cell(s) measured, {} unmeasured\n",
        reads.len(),
        cells.len(),
        unmeasured.len()
    );
    for c in &cells {
        println!(
            "{} / {} / {}: {} ({} reads of {})",
            c.task_type, c.profile, c.model, c.bar, c.reads, c.sample_count
        );
    }
    for u in &unmeasured {
        println!(
            "UNMEASURED {} / {} / {}: {}",
            u.task_type, u.profile, u.model, u.reason
        );
    }
    println!("\n{md}");
}

fn emit_cells(out_dir: &Path, date: &str) {
    let v: Value = serde_json::from_str(
        &fs::read_to_string(out_dir.join("cells.json"))
            .expect("cells.json (run --apply-ratings first)"),
    )
    .expect("cells.json parses");
    let cells: Vec<reading::CellOut> = v["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .map(|c| reading::CellOut {
            task_type: c["task_type"].as_str().unwrap().to_owned(),
            profile: c["profile"].as_str().unwrap().to_owned(),
            model: c["model"].as_str().unwrap().to_owned(),
            bar: c["bar"].as_str().unwrap().to_owned(),
            model_class: c["model_class"].as_str().unwrap().to_owned(),
            sample_count: c["sample_count"].as_u64().unwrap() as usize,
            reads: c["reads"].as_u64().unwrap() as usize,
            note: c["note"].as_str().unwrap().to_owned(),
        })
        .collect();
    println!(
        "-- shared_045.sql VALUES rows\n{}\n",
        reading::emit_sql(&cells, date)
    );
    println!(
        "-- catalog/provider-catalog.json \"capabilities\"\n{}",
        serde_json::to_string_pretty(&reading::emit_catalog(&cells, date)).expect("serialize")
    );
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
    let tags = judge_tags_of(&args);
    let with_voice = !args.no_voice;
    let with_writing = !args.voice_only;
    // A voice-only run has no writing briefs in its selection.
    let briefs = if with_writing { briefs } else { Vec::new() };
    assert!(
        !briefs.is_empty() || args.voice_only,
        "no brief matches --types"
    );

    if let Some(run) = &args.summarize {
        let md = write_outputs(&root.join(run), run, &briefs, &models, with_voice);
        println!("{md}");
        return;
    }
    if let Some(run) = &args.read_page {
        build_read_page(&root.join(run), run, &briefs, &models, with_voice, &tags);
        return;
    }
    if let Some((run, file)) = &args.apply_ratings {
        apply_ratings(&root, run, file, &briefs, &models, with_voice, &tags);
        return;
    }
    if let Some(run) = &args.emit_cells {
        let date = args
            .date
            .clone()
            .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
        emit_cells(&root.join(run), &date);
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
    let vcorpus = voice::corpus();
    let voice_cases = if with_voice {
        voice::all_cases(&vcorpus)
    } else {
        Vec::new()
    };
    let draft_count =
        (briefs.len() + voice_cases.len()) as u32 * models.len() as u32 * args.samples;
    println!(
        "{} writing briefs + {} voice cases x {} models x {} samples = {} drafts; output {}",
        briefs.len(),
        voice_cases.len(),
        models.len(),
        args.samples,
        draft_count,
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
    for (id, _mode, _t, _step, r) in &voice_cases {
        write(
            &out_dir.join("voice").join(id).join("prompt.txt"),
            &r.prompt,
        );
    }
    if args.dry_run {
        println!(
            "dry run: {} prompts rendered under {}; nothing generated",
            prompts.len() + voice_cases.len(),
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
        let target = Target {
            id: b.id.clone(),
            dir: b.id.clone(),
            group: b.document_type.clone(),
        };
        for m in &models {
            let opts = options_for(step, m);
            let finished = (1..=args.samples).all(|n| {
                out_dir
                    .join(&target.dir)
                    .join(format!("{}-{n}.json", slug(m)))
                    .exists()
            });
            if !finished {
                warm_up(&http, &base, m, &opts).await;
            }
            for n in 1..=args.samples {
                run_one(&http, &base, &out_dir, &target, m, n, &r.prompt, &opts).await;
            }
        }
        write_outputs(&out_dir, &run, &briefs, &models, with_voice);
        if bi == 0 {
            let first = brief_start.elapsed().as_secs_f64();
            let projected = first * prompts.len() as f64;
            println!(
                "\nTIMING after brief 1: {:.1} min ({} drafts). Projected writing run if the other briefs cost \
                 the same: {:.0} min ({:.0} min still to go). Long essays and stories cost more, so treat this as a floor.",
                first / 60.0,
                models.len() as u32 * args.samples,
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
    for (vi, (id, mode, _t, step, r)) in voice_cases.iter().enumerate() {
        println!("\n== voice {id} ({}/{})", vi + 1, voice_cases.len());
        let target = Target {
            id: id.clone(),
            dir: format!("voice/{id}"),
            group: mode.group().to_owned(),
        };
        for m in &models {
            let opts = options_for(step, m);
            let finished = (1..=args.samples).all(|n| {
                out_dir
                    .join(&target.dir)
                    .join(format!("{}-{n}.json", slug(m)))
                    .exists()
            });
            if !finished {
                warm_up(&http, &base, m, &opts).await;
            }
            for n in 1..=args.samples {
                run_one(&http, &base, &out_dir, &target, m, n, &r.prompt, &opts).await;
            }
        }
    }
    let md = write_outputs(&out_dir, &run, &briefs, &models, with_voice);

    if args.judge {
        let judge_base = args.judge_url.clone().unwrap_or_else(|| base.clone());
        let drafts = load_all(&out_dir, &briefs, &models, with_voice);
        println!("\njudging {} drafts with {:?}", drafts.len(), tags);
        let calls = judge::run_judges(&http, &judge_base, &out_dir, &drafts, &tags).await;
        println!("{calls} judge call(s) made");
        build_read_page(&out_dir, &run, &briefs, &models, with_voice, &tags);
    }
    sidecar.stop().await;
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
            id: format!("{brief}/m-1"),
            kind: Kind::Writing,
            group: "rma_letter".to_owned(),
            case: brief.to_owned(),
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
            text: "text".to_owned(),
            instructions: "instructions".to_owned(),
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

// ---------------------------------------------------------------------------
// Stage 3 tests: voice, judge, selection, read page, ratings, cells, emit
// ---------------------------------------------------------------------------

#[cfg(test)]
mod stage3 {
    use super::harness::judge::{
        judge_one, judge_prompt, judges_for, parse_verdict, JudgeRecord, Verdict as JVerdict,
    };
    use super::harness::reading::{
        agreement, build_cells, config, emit_catalog, emit_sql, order_hash, parse_ratings,
        read_html, rollup, select as pick, Config, Entry, Manifest, Read,
    };
    use super::harness::voice::{self, Mode};
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const MODELS3: [&str; 3] = ["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"];

    fn tags() -> Vec<String> {
        MODELS3.iter().map(|m| (*m).to_owned()).collect()
    }

    // -- voice ------------------------------------------------------------------

    #[test]
    fn the_voice_corpus_is_consistent_and_its_drafts_cover_their_own_points() {
        let c = voice::corpus();
        assert_eq!((c.apply.len(), c.refine.len()), (4, 3));
        for a in &c.apply {
            assert!(c.profiles.iter().any(|p| p.id == a.profile), "{}", a.id);
            let r = check_with(&a.points, &format!("{}\n", a.draft), &a.draft);
            assert!(
                r.missing_points.is_empty(),
                "{}: {:?}",
                a.id,
                r.missing_points
            );
            assert!(!r.has_findings(), "{}: {r:?}", a.id);
        }
        for r in &c.refine {
            assert!(c.profiles.iter().any(|p| p.id == r.profile), "{}", r.id);
        }
        // 84 voice drafts at the default four samples over three models.
        assert_eq!((c.apply.len() + c.refine.len()) * 3 * 4, 84);
    }

    #[test]
    fn every_voice_case_renders_through_the_shipped_voice_block() {
        let c = voice::corpus();
        let cases = voice::all_cases(&c);
        assert_eq!(cases.len(), 7);
        for (id, mode, _t, step, r) in &cases {
            assert_eq!(step.task_type, "quick_response", "{id}");
            let req = step.requirement.as_ref().expect("requirement");
            assert_eq!(req.block_stable_id, "cb-13");
            assert!(
                r.prompt.contains("directness="),
                "{id}: the profile is in the prompt"
            );
            match mode {
                Mode::Apply => {
                    let case = c.apply.iter().find(|a| &a.id == id).unwrap();
                    assert!(r.prompt.contains(&case.draft), "{id}");
                    assert!(r.allowed_text.contains(&case.draft));
                }
                Mode::Refine => assert!(!r.prompt.contains("Draft:"), "{id}"),
            }
        }
    }

    #[test]
    fn a_rewrite_that_adds_a_name_or_drops_a_fact_is_caught() {
        let c = voice::corpus();
        let a = c.apply.iter().find(|a| a.id == "apply-note").unwrap();
        let step = voice::expand_voice(document_type(&a.document_type), Mode::Apply);
        let r = voice::render_apply(&step, &c, a);
        let bad = "Hi Dorrie, thanks a lot for the tomatoes! We loved Berlin too.";
        let rep = check_with(&a.points, &r.allowed_text, bad);
        assert!(rep.unsupported.iter().any(|u| u == "Berlin"));
        assert!(!rep.missing_points.is_empty());
    }

    // -- the judge --------------------------------------------------------------

    fn good_reply(kind: Kind) -> String {
        let names: &[&str] = match kind {
            Kind::Writing => &[
                "covers_every_point",
                "adds_nothing_unsupported",
                "only_the_document",
                "right_form_and_length",
                "reads_naturally",
            ],
            Kind::VoiceApply => &[
                "keeps_every_fact",
                "adds_nothing_new",
                "only_the_rewrite",
                "matches_the_settings",
                "reads_naturally",
            ],
            Kind::VoiceRefine => &[
                "short_plain_guidance",
                "not_a_draft",
                "follows_the_settings",
                "invents_nothing",
                "only_the_guidance",
            ],
        };
        let c: Vec<String> = names.iter().map(|n| format!("\"{n}\": true")).collect();
        format!(
            "{{\"bar\": \"standard\", \"criteria\": {{{}}}}}",
            c.join(", ")
        )
    }

    #[test]
    fn a_wellformed_reply_parses_even_with_text_around_it() {
        for k in [Kind::Writing, Kind::VoiceApply, Kind::VoiceRefine] {
            let v = parse_verdict(&good_reply(k), k).expect("verdict");
            assert_eq!(v.bar, "standard");
            assert_eq!(v.criteria.len(), 5);
            let wrapped = format!(
                "Sure! Here you go:\n```json\n{}\n```\nHope that helps.",
                good_reply(k)
            );
            assert_eq!(parse_verdict(&wrapped, k), Some(v));
        }
    }

    #[test]
    fn a_reply_that_does_not_fit_the_rubric_is_no_verdict_never_a_guess() {
        let k = Kind::Writing;
        let ok = good_reply(k);
        for bad in [
            String::new(),
            "no json here".to_owned(),
            "{\"bar\": \"excellent\", \"criteria\": {}}".to_owned(),
            ok.replace("\"standard\"", "\"great\""),
            ok.replace(
                "\"covers_every_point\": true",
                "\"covers_every_point\": \"yes\"",
            ),
            ok.replace("\"covers_every_point\": true, ", ""),
            "{\"bar\": \"basic\"".to_owned(),
            "} {".to_owned(),
        ] {
            assert_eq!(parse_verdict(&bad, k), None, "{bad}");
        }
        // A criterion set for another kind does not satisfy this one.
        assert_eq!(
            parse_verdict(&good_reply(Kind::VoiceApply), Kind::Writing),
            None
        );
        // Extra criteria are dropped, a mixed-case bar is accepted.
        let extra = ok
            .replace("\"standard\"", "\"Standard\"")
            .replace("}}", ", \"bonus\": true}}");
        let v = parse_verdict(&extra, k).expect("verdict");
        assert_eq!(v.bar, "standard");
        assert!(!v.criteria.contains_key("bonus"));
    }

    #[test]
    fn the_judge_prompt_has_no_leftover_token_and_never_names_a_model() {
        for k in [Kind::Writing, Kind::VoiceApply, Kind::VoiceRefine] {
            let p = judge_prompt(k, "THE INSTRUCTIONS", "THE OUTPUT {output}");
            for t in ["{bars}", "{focus}", "{shape}", "{instructions}"] {
                assert!(!p.contains(t), "{k:?} leaves {t}");
            }
            assert!(p.contains("THE INSTRUCTIONS") && p.contains("THE OUTPUT"));
            assert!(p.contains("below_basic") && p.contains("\"criteria\""));
            for m in MODELS3 {
                assert!(!p.contains(m));
            }
        }
    }

    #[test]
    fn a_judge_never_grades_its_own_drafts() {
        let t = tags();
        let j = judges_for("llama3.1:8b", &t);
        assert_eq!(j.len(), 2);
        assert!(j.iter().all(|x| x.as_str() != "llama3.1:8b"));
        assert_eq!(judges_for("someone-else", &t).len(), 3);
    }

    /// A one-route Ollama stand-in on 127.0.0.1: every request gets `reply`.
    async fn mock_ollama(reply: String, hits: Arc<AtomicUsize>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    return;
                };
                let reply = reply.clone();
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_lowercase();
                        if let Some(h) = text.find("\r\n\r\n") {
                            let len = text
                                .split("content-length:")
                                .nth(1)
                                .and_then(|x| x.split("\r\n").next())
                                .and_then(|x| x.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= h + 4 + len {
                                break;
                            }
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let body = json!({"response": reply, "eval_count": 7, "load_duration": 0, "done_reason": "stop"})
                        .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn fake(group: &str, kind: Kind, case: &str, model: &str, n: u32, text: &str) -> Draft {
        let stem = format!("{}-{n}", slug(model));
        let id = match kind {
            Kind::Writing => format!("{case}/{stem}"),
            _ => format!("voice/{case}/{stem}"),
        };
        Draft {
            id,
            kind,
            group: group.to_owned(),
            case: case.to_owned(),
            rec: Record {
                brief: case.to_owned(),
                document_type: group.to_owned(),
                model: model.to_owned(),
                sample: n,
                words: text.split_whitespace().count(),
                latency_ms: 1000.0,
                load_ms: 0.0,
                output_tokens: 5,
                hit_cap: false,
                error: None,
            },
            text: text.to_owned(),
            instructions: "the instructions".to_owned(),
            report: CheckReport::default(),
            verdict: Verdict::Accept,
        }
    }

    #[tokio::test]
    async fn the_judge_call_parses_a_verdict_saves_it_and_reuses_it_without_a_call() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_ollama(good_reply(Kind::Writing), hits.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::new();
        let d = fake(
            "rma_letter",
            Kind::Writing,
            "rma_letter-1",
            "llama3.2:3b",
            1,
            "a draft",
        );
        let r = judge_one(&http, &base, dir.path(), "llama3.1:8b", &d).await;
        assert_eq!(r.verdict.as_ref().map(|v| v.bar.as_str()), Some("standard"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let again = judge_one(&http, &base, dir.path(), "llama3.1:8b", &d).await;
        assert_eq!(again, r);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a saved answer is not asked twice"
        );
        let saved = dir
            .path()
            .join("judge/llama3.1-8b/rma_letter-1__llama3.2-3b-1.json");
        assert!(saved.exists(), "{}", saved.display());
    }

    #[tokio::test]
    async fn garbage_and_a_dead_server_are_no_verdict_with_the_reason_kept() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = mock_ollama("I think it is fine".to_owned(), hits).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::new();
        let d = fake(
            "rma_letter",
            Kind::Writing,
            "rma_letter-1",
            "llama3.2:3b",
            1,
            "x",
        );
        let r = judge_one(&http, &base, dir.path(), "llama3.1:8b", &d).await;
        assert!(r.verdict.is_none() && r.error.is_none() && r.raw.contains("fine"));
        let dead = judge_one(&http, "http://127.0.0.1:9", dir.path(), "qwen2.5:7b", &d).await;
        assert!(dead.verdict.is_none() && dead.error.is_some());
    }

    #[test]
    fn a_judge_url_must_be_on_this_machine() {
        for ok in [
            "http://127.0.0.1:11434",
            "http://localhost:21434",
            "http://[::1]:1",
            "http://127.0.0.1:11434/",
            "http://localhost",
        ] {
            assert!(local_url(ok), "{ok}");
        }
        for bad in [
            "http://192.168.1.5:11434",
            "https://127.0.0.1:1",
            "http://example.com:80",
            "http://127.0.0.1.evil.com:80",
            "http://localhost:80@evil.com/",
            "http://127.0.0.1@evil.com:80/",
            "http://[::1]@evil.com:80/",
            "http://user:pw@127.0.0.1:11434/",
            "http://localhost.evil.com:80/",
            "http://0.0.0.0:11434",
            "ftp://127.0.0.1:21",
            "127.0.0.1:11434",
            "",
        ] {
            assert!(!local_url(bad), "{bad}");
        }
    }

    // -- selection --------------------------------------------------------------

    fn corpus_drafts(groups: &[&str], cases: usize, samples: u32) -> Vec<Draft> {
        let mut out = Vec::new();
        for g in groups {
            let kind = match *g {
                voice::GROUP_APPLY => Kind::VoiceApply,
                voice::GROUP_REFINE => Kind::VoiceRefine,
                _ => Kind::Writing,
            };
            for c in 1..=cases {
                for m in MODELS3 {
                    for n in 1..=samples {
                        out.push(fake(g, kind, &format!("{g}-{c}"), m, n, "some text here"));
                    }
                }
            }
        }
        out
    }

    const SIX: [&str; 6] = [
        "rma_letter",
        "complaint_email",
        "personal_note",
        "cover_letter",
        "essay_section",
        "creative_story",
    ];

    fn per_cell(m: &Manifest) -> BTreeMap<(String, String), usize> {
        let mut c = BTreeMap::new();
        for e in &m.entries {
            *c.entry((e.group.clone(), e.model.clone())).or_insert(0) += 1;
        }
        c
    }

    #[test]
    fn a_full_run_with_no_borderline_cells_is_three_reads_per_cell_72_in_all() {
        let mut groups: Vec<&str> = SIX.to_vec();
        groups.extend([voice::GROUP_APPLY, voice::GROUP_REFINE]);
        let ds = corpus_drafts(&groups, 3, 4);
        let m = pick("r", &ds, &BTreeMap::new(), &tags(), &config());
        assert_eq!(
            m.entries.len(),
            72,
            "18 writing cells and 6 voice cells, three reads each"
        );
        assert!(m.unreached.is_empty());
        assert!(per_cell(&m).values().all(|n| *n == 3));
        let ids: std::collections::BTreeSet<_> = m.entries.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), 72);
        // Reads of a cell come from different briefs where the cell has them.
        for ((g, model), _) in per_cell(&m) {
            let cases: std::collections::BTreeSet<_> = m
                .entries
                .iter()
                .filter(|e| e.group == g && e.model == model)
                .map(|e| e.id.split('/').rev().nth(1).unwrap().to_owned())
                .collect();
            assert_eq!(cases.len(), 3, "{g} {model}");
        }
        assert_eq!(m.cell_sizes["rma_letter|llama3.2:3b"], 12);
    }

    #[test]
    fn the_cap_is_never_exceeded_and_borderline_cells_are_reached_first() {
        let ds = corpus_drafts(&SIX, 3, 4);
        let mut cfg = config();
        cfg.reads.max_total = 10; // room for three cells and one read over
                                  // Make cover_letter / qwen borderline: a quarter of its drafts have findings.
        let mut ds = ds;
        for d in ds
            .iter_mut()
            .filter(|d| d.group == "cover_letter" && d.rec.model == "qwen2.5:7b")
            .take(3)
        {
            d.report.placeholders.push("[x]".to_owned());
        }
        let m = pick("r", &ds, &BTreeMap::new(), &tags(), &cfg);
        // Three cells take their base reads (9) and the borderline cell gets the one
        // extra read the cap leaves.
        assert_eq!(m.entries.len(), 10);
        assert_eq!(
            per_cell(&m).get(&("cover_letter".to_owned(), "qwen2.5:7b".to_owned())),
            Some(&4)
        );
        assert_eq!(m.unreached.len(), 18 - per_cell(&m).len());
        assert!(!m.unreached.contains(&"cover_letter|qwen2.5:7b".to_owned()));
        for u in &m.unreached {
            assert!(!m
                .entries
                .iter()
                .any(|e| &super::harness::reading::cell_key(&e.group, &e.model) == u));
        }
    }

    #[test]
    fn a_borderline_cell_gets_extra_reads_up_to_six_and_others_stay_at_three() {
        let mut ds = corpus_drafts(&["rma_letter", "cover_letter"], 3, 4);
        // 4 of 12 cover_letter/llama3.2 drafts have findings: 33%, inside the band.
        for d in ds
            .iter_mut()
            .filter(|d| d.group == "cover_letter" && d.rec.model == "llama3.2:3b")
            .take(4)
        {
            d.report.unsupported.push("Leeds".to_owned());
        }
        let m = pick("r", &ds, &BTreeMap::new(), &tags(), &config());
        let c = per_cell(&m);
        assert_eq!(c[&("cover_letter".to_owned(), "llama3.2:3b".to_owned())], 6);
        assert_eq!(c[&("cover_letter".to_owned(), "llama3.1:8b".to_owned())], 3);
        assert_eq!(c[&("rma_letter".to_owned(), "llama3.2:3b".to_owned())], 3);
        // The extras are the flagged drafts, with the reason written down.
        let extra: Vec<&Entry> = m
            .entries
            .iter()
            .filter(|e| e.reasons[0].contains("findings"))
            .collect();
        assert_eq!(extra.len(), 3);
    }

    #[test]
    fn judge_disagreement_makes_a_cell_borderline_and_selection_is_deterministic() {
        let ds = corpus_drafts(&["rma_letter"], 3, 4);
        let mut v: BTreeMap<String, Vec<JudgeRecord>> = BTreeMap::new();
        for d in ds.iter().filter(|d| d.rec.model == "qwen2.5:7b") {
            let mk = |judge: &str, bar: &str| JudgeRecord {
                draft: d.id.clone(),
                judge: judge.to_owned(),
                verdict: Some(JVerdict {
                    bar: bar.to_owned(),
                    criteria: BTreeMap::new(),
                }),
                raw: String::new(),
                error: None,
            };
            v.insert(
                d.id.clone(),
                vec![mk("llama3.2:3b", "basic"), mk("llama3.1:8b", "high")],
            );
        }
        let a = pick("r", &ds, &v, &tags(), &config());
        let b = pick("r", &ds, &v, &tags(), &config());
        assert_eq!(a, b);
        let q = per_cell(&a)[&("rma_letter".to_owned(), "qwen2.5:7b".to_owned())];
        assert_eq!(
            q, 6,
            "disagreement scores every draft, so the cell gets its extras"
        );
    }

    #[test]
    fn errored_drafts_are_never_selected_and_a_small_cell_is_read_whole() {
        let mut ds = corpus_drafts(&["rma_letter"], 1, 2);
        for d in ds.iter_mut().filter(|d| d.rec.model == "llama3.2:3b") {
            d.rec.error = Some("boom".to_owned());
        }
        let m = pick("r", &ds, &BTreeMap::new(), &tags(), &config());
        assert!(!m.entries.iter().any(|e| e.model == "llama3.2:3b"));
        assert_eq!(
            per_cell(&m)[&("rma_letter".to_owned(), "llama3.1:8b".to_owned())],
            2
        );
    }

    #[test]
    fn the_display_order_hash_is_fixed() {
        assert_eq!(order_hash("a"), order_hash("a"));
        assert_ne!(order_hash("a"), order_hash("b"));
        assert_eq!(order_hash(""), 0xcbf29ce484222325);
    }

    // -- read.html --------------------------------------------------------------

    fn manifest_of(ds: &[Draft]) -> Manifest {
        pick("run-x", ds, &BTreeMap::new(), &tags(), &config())
    }

    #[test]
    fn the_page_says_the_exact_count_and_the_cap_before_the_first_draft() {
        let ds = corpus_drafts(&["rma_letter", "cover_letter"], 3, 4);
        let m = manifest_of(&ds);
        let html = read_html(&m, &ds, &BTreeMap::new());
        let first = html.find("<section").unwrap();
        let count = html.find("18 drafts to read").expect("count line");
        assert!(count < first);
        assert!(html[..first].contains("limit 100"));
        assert!(html.contains("0 of 18 rated") || html.contains("<span id=progress>0 of 18 rated"));
        assert_eq!(html.matches("<section data-id").count(), 18);
        assert!(html.contains("Export ratings.json"));
    }

    #[test]
    fn the_page_escapes_every_draft_and_hides_the_model() {
        let mut ds = corpus_drafts(&["rma_letter"], 3, 4);
        let hostile = "</pre><script>alert(1)</script> \"quoted\" & <b>";
        for d in ds.iter_mut() {
            d.text = hostile.to_owned();
            d.instructions = hostile.to_owned();
            d.report
                .unsupported
                .push("<img src=x onerror=1>".to_owned());
        }
        let m = manifest_of(&ds);
        let html = read_html(&m, &ds, &BTreeMap::new());
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        // The only scripts are the page's own: one.
        assert_eq!(html.matches("<script>").count(), 1);
        // Headings do not name a model.
        for line in html.split("<h3>").skip(1) {
            let h = line.split("</h3>").next().unwrap();
            for t in MODELS3 {
                assert!(!h.contains(t) && !h.contains(&slug(t)));
            }
        }
    }

    #[test]
    fn judge_verdicts_are_folded_away_and_unreached_cells_are_listed() {
        let ds = corpus_drafts(&SIX, 3, 4);
        let mut cfg = config();
        cfg.reads.max_total = 6;
        let m = pick("run-x", &ds, &BTreeMap::new(), &tags(), &cfg);
        let mut v = BTreeMap::new();
        let e = &m.entries[0];
        v.insert(
            e.id.clone(),
            vec![JudgeRecord {
                draft: e.id.clone(),
                judge: "j-model".to_owned(),
                verdict: Some(JVerdict {
                    bar: "high".to_owned(),
                    criteria: BTreeMap::new(),
                }),
                raw: String::new(),
                error: None,
            }],
        );
        let html = read_html(&m, &ds, &v);
        assert!(html.contains("<details><summary>Judge verdicts (open after you rate)"));
        assert!(html.contains("judge j-model: <b>high</b>"));
        assert!(
            html.contains("16 cell(s) not reached"),
            "{}",
            &html[..html.find("<section").unwrap()]
        );
        assert!(html.contains("2 drafts") || html.contains("6 drafts to read"));
    }

    // -- ratings, cells, agreement, emit ----------------------------------------

    fn entry(id: &str, group: &str, model: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            group: group.to_owned(),
            model: model.to_owned(),
            reasons: vec!["base read".to_owned()],
        }
    }

    fn manifest_with(entries: Vec<Entry>, sizes: &[(&str, usize)]) -> Manifest {
        Manifest {
            run: "run-x".to_owned(),
            max_total: 100,
            entries,
            unreached: vec![],
            cell_sizes: sizes.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect(),
        }
    }

    #[test]
    fn rollup_takes_the_best_bar_that_two_thirds_of_the_reads_reach() {
        let s = 0.66;
        assert_eq!(rollup(&["standard"], s), Some("standard"));
        assert_eq!(rollup(&["high", "basic"], s), Some("basic"));
        assert_eq!(rollup(&["high", "standard", "basic"], s), Some("standard"));
        assert_eq!(rollup(&["high", "high", "below_basic"], s), Some("high"));
        assert_eq!(rollup(&["basic", "basic", "below_basic"], s), Some("basic"));
        assert_eq!(
            rollup(&["below_basic", "below_basic", "high"], s),
            Some("below_basic")
        );
        assert_eq!(
            rollup(&["high", "high", "standard", "basic"], s),
            Some("standard")
        );
        assert_eq!(
            rollup(&["high", "high", "high", "standard", "basic"], s),
            Some("standard")
        );
        assert_eq!(rollup(&[], s), None);
        assert_eq!(rollup(&["nonsense"], s), None);
    }

    #[test]
    fn ratings_are_checked_against_the_manifest() {
        let cfg = config();
        let m = manifest_with(
            vec![
                entry("a/x-1", "rma_letter", "m"),
                entry("a/x-2", "rma_letter", "m"),
            ],
            &[],
        );
        let ok = r#"{"run":"run-x","reads":[{"id":"a/x-1","bar":"basic","note":"fine"},{"id":"a/x-2","bar":"high"}]}"#;
        let reads = parse_ratings(ok, &m, &cfg).unwrap();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[1].note, "");
        for (bad, why) in [
            (r#"{"run":"other","reads":[]}"#, "run"),
            (
                r#"{"run":"run-x","reads":[{"id":"a/x-1","bar":"great"}]}"#,
                "not a bar",
            ),
            (
                r#"{"run":"run-x","reads":[{"id":"zzz","bar":"basic"}]}"#,
                "not a draft",
            ),
            (
                r#"{"run":"run-x","reads":[{"id":"a/x-1","bar":"basic"},{"id":"a/x-1","bar":"high"}]}"#,
                "twice",
            ),
            ("not json", "ratings.json"),
        ] {
            let e = parse_ratings(bad, &m, &cfg).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }

    fn read(id: &str, bar: &str) -> Read {
        Read {
            id: id.to_owned(),
            bar: bar.to_owned(),
            note: String::new(),
        }
    }

    #[test]
    fn a_writing_cell_needs_three_reads_and_carries_jasons_bar_only() {
        let cfg = config();
        let ents: Vec<Entry> = (1..=3)
            .map(|i| entry(&format!("c/m-{i}"), "rma_letter", "llama3.2:3b"))
            .collect();
        let m = manifest_with(ents, &[("rma_letter|llama3.2:3b", 12)]);
        let reads = vec![
            read("c/m-1", "basic"),
            read("c/m-2", "standard"),
            read("c/m-3", "basic"),
        ];
        let (cells, un) = build_cells(&reads, &m, &cfg).unwrap();
        assert!(un.is_empty());
        let c = &cells[0];
        assert_eq!(
            (c.task_type.as_str(), c.profile.as_str(), c.bar.as_str()),
            ("creative_writing", "rma_letter", "basic")
        );
        assert_eq!(
            (c.model_class.as_str(), c.sample_count, c.reads),
            ("small", 12, 3)
        );
        assert!(c.note.contains("Read 3 of 12") && !c.note.contains(';'));
        // Two reads are not enough: the cell stays unmeasured.
        let (cells, un) = build_cells(&reads[..2], &m, &cfg).unwrap();
        assert!(cells.is_empty());
        assert_eq!(un[0].reason, "2 reads, 3 needed");
        // No read at all: unmeasured too.
        let (cells, un) = build_cells(&[], &m, &cfg).unwrap();
        assert!(cells.is_empty() && un.len() == 1);
    }

    #[test]
    fn the_voice_cell_is_task_type_wide_and_takes_the_lower_mode() {
        let cfg = config();
        let mut ents = Vec::new();
        for i in 1..=3 {
            ents.push(entry(
                &format!("voice/a/m-{i}"),
                voice::GROUP_APPLY,
                "qwen2.5:7b",
            ));
            ents.push(entry(
                &format!("voice/r/m-{i}"),
                voice::GROUP_REFINE,
                "qwen2.5:7b",
            ));
        }
        let m = manifest_with(
            ents,
            &[
                ("voice_apply|qwen2.5:7b", 16),
                ("voice_refine|qwen2.5:7b", 12),
            ],
        );
        let reads: Vec<Read> = (1..=3)
            .flat_map(|i| {
                [
                    read(&format!("voice/a/m-{i}"), "standard"),
                    read(&format!("voice/r/m-{i}"), "basic"),
                ]
            })
            .collect();
        let (cells, un) = build_cells(&reads, &m, &cfg).unwrap();
        assert!(un.is_empty());
        assert_eq!(cells.len(), 1);
        let c = &cells[0];
        assert_eq!(
            (c.task_type.as_str(), c.profile.as_str(), c.bar.as_str()),
            ("quick_response", "", "basic")
        );
        assert_eq!(
            (c.sample_count, c.reads, c.model_class.as_str()),
            (28, 6, "medium")
        );
        assert!(c.note.contains("apply standard, refine basic"));
        // One mode short of reads leaves the whole voice cell unmeasured.
        let (cells, un) = build_cells(&reads[..5], &m, &cfg).unwrap();
        assert!(cells.is_empty());
        assert!(un[0].reason.starts_with("voice_refine"), "{}", un[0].reason);
    }

    #[test]
    fn a_model_with_no_class_in_bars_json_is_an_error_not_a_default() {
        let cfg = config();
        let ents: Vec<Entry> = (1..=3)
            .map(|i| entry(&format!("c/m-{i}"), "rma_letter", "mystery:1b"))
            .collect();
        let m = manifest_with(ents, &[("rma_letter|mystery:1b", 3)]);
        let reads: Vec<Read> = (1..=3)
            .map(|i| read(&format!("c/m-{i}"), "basic"))
            .collect();
        assert!(build_cells(&reads, &m, &cfg)
            .unwrap_err()
            .contains("model_class"));
    }

    #[test]
    fn agreement_counts_the_judge_against_the_reads() {
        let mk = |id: &str, judge: &str, bar: Option<&str>| JudgeRecord {
            draft: id.to_owned(),
            judge: judge.to_owned(),
            verdict: bar.map(|b| JVerdict {
                bar: b.to_owned(),
                criteria: BTreeMap::new(),
            }),
            raw: String::new(),
            error: None,
        };
        let mut v = BTreeMap::new();
        v.insert(
            "a".to_owned(),
            vec![mk("a", "j1", Some("basic")), mk("a", "j2", Some("high"))],
        );
        v.insert(
            "b".to_owned(),
            vec![mk("b", "j1", Some("below_basic")), mk("b", "j2", None)],
        );
        let reads = vec![
            read("a", "basic"),
            read("b", "standard"),
            read("c", "basic"),
        ];
        let a = agreement(&reads, &v);
        let j1 = a.iter().find(|x| x.judge == "j1").unwrap();
        assert_eq!(
            (
                j1.compared,
                j1.exact,
                j1.within_one,
                j1.judge_lower,
                j1.judge_higher
            ),
            (2, 1, 1, 1, 0)
        );
        let j2 = a.iter().find(|x| x.judge == "j2").unwrap();
        assert_eq!(
            (j2.compared, j2.exact, j2.within_one, j2.judge_higher),
            (1, 0, 0, 1)
        );
    }

    fn sample_cells() -> Vec<super::harness::reading::CellOut> {
        use super::harness::reading::CellOut;
        vec![
            CellOut {
                task_type: "creative_writing".to_owned(),
                profile: "rma_letter".to_owned(),
                model: "llama3.2:3b".to_owned(),
                bar: "basic".to_owned(),
                model_class: "small".to_owned(),
                sample_count: 12,
                reads: 3,
                note: "Read 3 of 12 drafts: 3 basic.".to_owned(),
            },
            CellOut {
                task_type: "quick_response".to_owned(),
                profile: String::new(),
                model: "qwen2.5:7b".to_owned(),
                bar: "below_basic".to_owned(),
                model_class: "medium".to_owned(),
                sample_count: 28,
                reads: 6,
                note: "Voice step, the lower bar. It's 6 reads.".to_owned(),
            },
        ]
    }

    #[test]
    fn the_catalog_json_is_a_valid_capability_doc_and_the_sql_loads_into_the_real_table() {
        use quietrabbit_lib::provider_catalog::format::CapabilityDoc;
        let cells = sample_cells();
        for v in emit_catalog(&cells, "2026-11-20").as_array().unwrap() {
            let doc: CapabilityDoc = serde_json::from_value(v.clone()).expect("a CapabilityDoc");
            assert_eq!(doc.measured_at, "2026-11-20");
        }
        // The wide cell has no profile key at all.
        assert!(emit_catalog(&cells, "d")[1].get("profile").is_none());

        let schema = include_str!("../schema/shared_045.sql");
        let create_start = schema
            .find("CREATE TABLE IF NOT EXISTS model_capabilities")
            .unwrap();
        let create =
            &schema[create_start..create_start + schema[create_start..].find(");").unwrap() + 2];
        let sql = format!(
            "INSERT OR IGNORE INTO model_capabilities (subject_kind, provider_id, model_id, task_type, profile, \
             attained_bar, model_class, sample_count, measured_at, provenance, note) VALUES\n{}",
            emit_sql(&cells, "2026-11-20")
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use sqlx::{Connection, Row};
            let mut c = sqlx::SqliteConnection::connect("sqlite::memory:")
                .await
                .unwrap();
            sqlx::query(create).execute(&mut c).await.unwrap();
            sqlx::query(&sql)
                .execute(&mut c)
                .await
                .expect("the emitted rows satisfy the table's checks");
            let rows = sqlx::query(
                "SELECT task_type, profile, note FROM model_capabilities ORDER BY task_type",
            )
            .fetch_all(&mut c)
            .await
            .unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].get::<String, _>("task_type"), "creative_writing");
            assert_eq!(rows[1].get::<String, _>("profile"), "");
            // The apostrophe in the note survived the quoting.
            assert!(rows[1].get::<String, _>("note").contains("It's 6 reads"));
        });
    }

    #[test]
    #[should_panic(expected = "semicolon")]
    fn a_note_with_a_semicolon_is_refused_by_the_sql_emitter() {
        let mut c = sample_cells();
        c[0].note = "a; b".to_owned();
        emit_sql(&c, "d");
    }

    #[test]
    fn bars_json_agrees_with_the_shipped_vocabulary_and_the_seed_model_classes() {
        let cfg: Config = config();
        use quietrabbit_lib::conductor::model_capability::AttainedBar;
        for b in &cfg.vocabulary {
            assert!(
                serde_json::from_value::<AttainedBar>(json!(b)).is_ok(),
                "{b}"
            );
        }
        // Where shared_045.sql already holds a model's class, bars.json says the same.
        let seed = include_str!("../schema/shared_045.sql");
        for (m, class) in &cfg.model_class {
            for line in seed.lines().filter(|l| l.contains(&format!("'{m}'"))) {
                let _ = line;
            }
            let row_pos = seed.find(&format!("'{m}'")).expect("model in seed");
            let tail = &seed[row_pos..row_pos + 200];
            assert!(tail.contains(&format!("'{class}'")), "{m}: {class}");
        }
    }

    // -- the whole flow on a fake run --------------------------------------------

    fn write_fake(dir: &Path, rel: &str, model: &str, n: u32, group: &str, text: &str) {
        let stem = format!("{}-{n}", slug(model));
        write(&dir.join(rel).join(format!("{stem}.md")), text);
        let rec = Record {
            brief: rel.to_owned(),
            document_type: group.to_owned(),
            model: model.to_owned(),
            sample: n,
            words: text.split_whitespace().count(),
            latency_ms: 1000.0,
            load_ms: 0.0,
            output_tokens: 5,
            hit_cap: false,
            error: None,
        };
        write(
            &dir.join(rel).join(format!("{stem}.json")),
            &serde_json::to_string(&rec).unwrap(),
        );
    }

    #[test]
    fn a_fake_run_goes_from_drafts_to_read_page_to_ratings_to_cells_to_emit() {
        let root = tempfile::tempdir().unwrap();
        let run = "run-fake";
        let out = root.path().join(run);
        let briefs = select(
            &corpus(),
            &Some(vec!["rma_letter".to_owned(), "complaint_email".to_owned()]),
            1,
        );
        assert_eq!(briefs.len(), 2);
        let models = tags();
        for b in &briefs {
            let restated = b
                .points
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            for m in &models {
                for n in 1..=3 {
                    write_fake(&out, &b.id, m, n, &b.document_type, &restated);
                }
            }
        }
        let vc = voice::corpus();
        for a in &vc.apply {
            for m in &models {
                for n in 1..=3 {
                    write_fake(
                        &out,
                        &format!("voice/{}", a.id),
                        m,
                        n,
                        voice::GROUP_APPLY,
                        &a.draft,
                    );
                }
            }
        }
        for r in &vc.refine {
            for m in &models {
                for n in 1..=3 {
                    write_fake(
                        &out,
                        &format!("voice/{}", r.id),
                        m,
                        n,
                        voice::GROUP_REFINE,
                        "Keep it brisk and plain.",
                    );
                }
            }
        }
        let manifest = build_read_page(&out, run, &briefs, &models, true, &tags());
        // 2 writing types + 2 voice modes, 3 models each, 3 reads per cell.
        assert_eq!(manifest.entries.len(), 4 * 3 * 3);
        assert!(out.join("read.html").exists() && out.join("read_manifest.json").exists());
        let html = fs::read_to_string(out.join("read.html")).unwrap();
        assert!(html.contains("36 drafts to read"));

        let reads: Vec<Value> = manifest
            .entries
            .iter()
            .map(|e| json!({"id": e.id, "bar": "basic"}))
            .collect();
        let file = root.path().join("ratings.json");
        fs::write(&file, json!({"run": run, "reads": reads}).to_string()).unwrap();
        apply_ratings(root.path(), run, &file, &briefs, &models, true, &tags());
        let cells: Value =
            serde_json::from_str(&fs::read_to_string(out.join("cells.json")).unwrap()).unwrap();
        let cs = cells["cells"].as_array().unwrap();
        assert_eq!(
            cs.len(),
            2 * 3 + 3,
            "six writing cells and one voice cell per model"
        );
        assert!(cs.iter().all(|c| c["bar"] == "basic"));
        assert!(cells["unmeasured"].as_array().unwrap().is_empty());
        assert!(out.join("agreement.md").exists() && out.join("ratings.json").exists());
        assert_eq!(
            cs.iter()
                .filter(|c| c["task_type"] == "quick_response")
                .count(),
            3
        );
    }
}

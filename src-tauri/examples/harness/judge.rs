// src-tauri/examples/harness/judge.rs
//
// DEV-ONLY (items.id=899 stage 3). The developer-run rubric judge. It is a LOCAL
// model asked to put a draft at one of four bars and tick a few criteria. Its only
// jobs are to choose which drafts Jason reads (harness/reading.rs) and to be
// reported against his reads. It never sets a bar in the baseline catalog
// (decisions.id=950 ruling 6).
//
//   - The judge is chosen on the command line (--judge-tags, default: the other
//     bundled models) and never an author's own model, so a model does not grade
//     itself. With the default two judge tags per draft, two passes exist and their
//     disagreement is a reason to read the draft.
//   - Temperature 0, a short answer, one JSON object. A reply that does not parse
//     to a known bar and every named criterion is NO VERDICT, never a guess.
//   - The judge sees the instructions the writer was given and the writer's output.
//     It is never told which model wrote it.
//   - Synthetic data only; the one network peer is a local Ollama.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::support::{generate, slug, write};
use crate::{Draft, Kind};

const RUBRICS_JSON: &str = include_str!("../calibration_data/rubrics.json");
const BARS: [&str; 4] = ["below_basic", "basic", "standard", "high"];

#[derive(Deserialize)]
struct Rubrics {
    bars: String,
    template: String,
    kinds: BTreeMap<String, KindRubric>,
}

#[derive(Deserialize)]
struct KindRubric {
    focus: String,
    criteria: Vec<String>,
}

fn rubrics() -> Rubrics {
    serde_json::from_str(RUBRICS_JSON).expect("calibration_data/rubrics.json parses")
}

pub fn bar_rank(bar: &str) -> Option<usize> {
    BARS.iter().position(|b| *b == bar)
}

fn kind_key(k: Kind) -> &'static str {
    match k {
        Kind::Writing => "writing",
        Kind::VoiceApply => "voice_apply",
        Kind::VoiceRefine => "voice_refine",
    }
}

/// The exact JSON shape the judge is asked for.
fn shape(criteria: &[String]) -> String {
    let c: Vec<String> = criteria
        .iter()
        .map(|n| format!("\"{n}\": true or false"))
        .collect();
    format!(
        "{{\"bar\": \"below_basic or basic or standard or high\", \"criteria\": {{{}}}}}",
        c.join(", ")
    )
}

pub fn judge_prompt(kind: Kind, instructions: &str, output: &str) -> String {
    let r = rubrics();
    let k = r.kinds.get(kind_key(kind)).expect("rubric for kind");
    // Same rule as the executor's render: replace each {token} with String::replace.
    // The writer's output goes in last, so text inside it is never re-read as a token.
    let mut s = r.template.clone();
    for (name, value) in [
        ("bars", r.bars.as_str()),
        ("focus", k.focus.as_str()),
        ("shape", &shape(&k.criteria)),
        ("instructions", instructions),
        ("output", output),
    ] {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Verdict {
    pub bar: String,
    pub criteria: BTreeMap<String, bool>,
}

/// Reads the judge's reply. The first `{` to its matching `}` is the object; a
/// reply with no object, an unknown bar or a missing or non-boolean criterion is
/// None. Extra criteria are dropped.
pub fn parse_verdict(raw: &str, kind: Kind) -> Option<Verdict> {
    let r = rubrics();
    let wanted = &r.kinds.get(kind_key(kind))?.criteria;
    let start = raw.find('{')?;
    let mut depth = 0usize;
    let mut end = None;
    for (i, c) in raw[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let v: Value = serde_json::from_str(&raw[start..end?]).ok()?;
    let bar = v.get("bar")?.as_str()?.trim().to_lowercase();
    bar_rank(&bar)?;
    let crit = v.get("criteria")?.as_object()?;
    let mut criteria = BTreeMap::new();
    for name in wanted {
        criteria.insert(name.clone(), crit.get(name)?.as_bool()?);
    }
    Some(Verdict { bar, criteria })
}

pub fn judge_options(model: &str) -> Value {
    json!({
        "temperature": 0.0,
        "num_predict": 300,
        "num_ctx": match model {
            "llama3.2:3b" => 4096,
            "llama3.1:8b" | "qwen2.5:7b" => 8192,
            _ => 4096,
        }
    })
}

/// One judge's saved answer for one draft.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JudgeRecord {
    pub draft: String,
    pub judge: String,
    pub verdict: Option<Verdict>,
    /// The reply, cut to 1500 characters, kept so a no-verdict can be read.
    pub raw: String,
    pub error: Option<String>,
}

fn record_path(out_dir: &Path, judge: &str, draft_id: &str) -> PathBuf {
    out_dir
        .join("judge")
        .join(slug(judge))
        .join(format!("{}.json", draft_id.replace('/', "__")))
}

/// The judge tags that may grade a draft: every tag except its author's.
pub fn judges_for<'a>(author: &str, tags: &'a [String]) -> Vec<&'a String> {
    tags.iter().filter(|t| t.as_str() != author).collect()
}

/// Asks one judge about one draft, or reuses its saved answer.
pub async fn judge_one(
    http: &reqwest::Client,
    base_url: &str,
    out_dir: &Path,
    judge: &str,
    d: &Draft,
) -> JudgeRecord {
    let path = record_path(out_dir, judge, &d.id);
    if let Ok(s) = fs::read_to_string(&path) {
        if let Ok(r) = serde_json::from_str::<JudgeRecord>(&s) {
            return r;
        }
    }
    let prompt = judge_prompt(d.kind, &d.instructions, &d.text);
    let rec = match generate(http, base_url, judge, &prompt, &judge_options(judge)).await {
        Ok(g) => JudgeRecord {
            draft: d.id.clone(),
            judge: judge.to_owned(),
            verdict: parse_verdict(&g.text, d.kind),
            raw: g.text.chars().take(1500).collect(),
            error: None,
        },
        Err(e) => JudgeRecord {
            draft: d.id.clone(),
            judge: judge.to_owned(),
            verdict: None,
            raw: String::new(),
            error: Some(e),
        },
    };
    write(
        &path,
        &serde_json::to_string_pretty(&rec).expect("serialize judge record"),
    );
    rec
}

/// Judge pass: judge outer loop (one model load per judge), draft inner. A draft
/// that errored at generation (empty text) is not judged. Returns how many judge
/// calls were made (saved answers are not counted).
pub async fn run_judges(
    http: &reqwest::Client,
    base_url: &str,
    out_dir: &Path,
    drafts: &[Draft],
    tags: &[String],
) -> usize {
    let mut calls = 0;
    for judge in tags {
        let todo: Vec<&Draft> = drafts
            .iter()
            .filter(|d| !d.errored() && d.rec.model != *judge)
            .filter(|d| !record_path(out_dir, judge, &d.id).exists())
            .collect();
        println!("  judge {judge}: {} drafts to judge", todo.len());
        for d in todo {
            let r = judge_one(http, base_url, out_dir, judge, d).await;
            calls += 1;
            if r.verdict.is_none() {
                println!(
                    "    {}: no verdict{}",
                    d.id,
                    r.error
                        .as_deref()
                        .map_or(String::new(), |e| format!(" ({e})"))
                );
            }
        }
    }
    calls
}

/// Every saved verdict of every draft, by draft id, judges in tag order.
pub fn load_verdicts(
    out_dir: &Path,
    drafts: &[Draft],
    tags: &[String],
) -> BTreeMap<String, Vec<JudgeRecord>> {
    let mut out: BTreeMap<String, Vec<JudgeRecord>> = BTreeMap::new();
    for d in drafts {
        for j in judges_for(&d.rec.model, tags) {
            if let Ok(s) = fs::read_to_string(record_path(out_dir, j, &d.id)) {
                if let Ok(r) = serde_json::from_str::<JudgeRecord>(&s) {
                    out.entry(d.id.clone()).or_default().push(r);
                }
            }
        }
    }
    out
}

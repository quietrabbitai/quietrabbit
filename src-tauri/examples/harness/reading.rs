// src-tauri/examples/harness/reading.rs
//
// DEV-ONLY (items.id=899 stage 3, decisions.id=950 rulings 6 and 7). Everything
// between the drafts and a baseline cell:
//
//   select        which drafts Jason reads: at least min_per_cell for every cell
//                 (so the judge can be reported against him), more for borderline
//                 cells, never more than max_total (100). Cells the cap does not
//                 reach stay unmeasured.
//   read.html     one static page: the exact draft count first, then the drafts in
//                 a fixed shuffled order with the model hidden, four bar buttons
//                 and a note each, progress in the browser's localStorage, and an
//                 Export button that downloads ratings.json. No server.
//   ratings       --apply-ratings validates the file against the manifest.
//   cells         a cell bar is Jason's reads rolled up (bars.json): the highest
//                 bar that at least `share_at_or_above` of the reads reach. No cell
//                 is written from a judge's bar, and a cell with fewer than
//                 min_per_cell reads is unmeasured.
//   agreement     the judge against Jason, per judge.
//   emit          the SQL VALUES block for shared_045.sql and the catalog
//                 `capabilities` array, provenance jason_read.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::harness::judge::{bar_rank, JudgeRecord};
use crate::harness::voice::{GROUP_APPLY, GROUP_REFINE};
use crate::{esc, Draft};

const BARS_JSON: &str = include_str!("../calibration_data/bars.json");

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub vocabulary: Vec<String>,
    pub reads: ReadLimits,
    pub borderline: Borderline,
    pub rollup: Rollup,
    pub model_class: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ReadLimits {
    pub min_per_cell: usize,
    pub max_per_cell: usize,
    pub max_total: usize,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Borderline {
    pub finding_rate_band: [f64; 2],
    pub judge_disagreement_share: f64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Rollup {
    pub share_at_or_above: f64,
    pub quick_response: String,
}

pub fn config() -> Config {
    let c: Config = serde_json::from_str(BARS_JSON).expect("calibration_data/bars.json parses");
    assert_eq!(
        c.vocabulary,
        ["below_basic", "basic", "standard", "high"],
        "bars.json vocabulary must be the model_capabilities one"
    );
    assert!(c.reads.min_per_cell >= 1 && c.reads.max_per_cell >= c.reads.min_per_cell);
    assert_eq!(c.rollup.quick_response, "min_over_modes");
    c
}

pub fn cell_key(group: &str, model: &str) -> String {
    format!("{group}|{model}")
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub group: String,
    pub model: String,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub run: String,
    pub max_total: usize,
    pub entries: Vec<Entry>,
    /// Cells the cap did not reach, as "group|model".
    pub unreached: Vec<String>,
    /// Non-errored drafts per cell, as "group|model".
    pub cell_sizes: BTreeMap<String, usize>,
}

fn disagrees(vs: &[JudgeRecord], expected: usize) -> bool {
    let bars: Vec<usize> = vs
        .iter()
        .filter_map(|v| v.verdict.as_ref().and_then(|x| bar_rank(&x.bar)))
        .collect();
    bars.len() < expected || bars.windows(2).any(|w| w[0] != w[1])
}

fn expected_judges(d: &Draft, tags: &[String]) -> usize {
    tags.iter().filter(|t| **t != d.rec.model).count()
}

fn item_score(d: &Draft, vs: &[JudgeRecord], expected: usize) -> usize {
    let mut s = 0;
    if expected > 0 && disagrees(vs, expected) {
        s += 3;
    }
    if d.report.has_findings() {
        s += 2;
    }
    if vs
        .iter()
        .any(|v| v.verdict.as_ref().is_some_and(|x| x.bar == "basic"))
    {
        s += 1;
    }
    s
}

/// Why a cell is borderline, or empty.
pub fn borderline_reasons(
    cell: &[&Draft],
    verdicts: &BTreeMap<String, Vec<JudgeRecord>>,
    tags: &[String],
    cfg: &Config,
) -> Vec<String> {
    let n = cell.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let findings = cell.iter().filter(|d| d.report.has_findings()).count();
    let rate = findings as f64 / n as f64;
    let [lo, hi] = cfg.borderline.finding_rate_band;
    if rate >= lo && rate <= hi {
        out.push(format!("{:.0}% of the drafts have findings", rate * 100.0));
    }
    let judged: Vec<&&Draft> = cell
        .iter()
        .filter(|d| expected_judges(d, tags) > 0)
        .collect();
    if !judged.is_empty() {
        let dis = judged
            .iter()
            .filter(|d| {
                disagrees(
                    verdicts.get(&d.id).map_or(&[][..], Vec::as_slice),
                    expected_judges(d, tags),
                )
            })
            .count();
        let share = dis as f64 / judged.len() as f64;
        if share >= cfg.borderline.judge_disagreement_share {
            out.push(format!(
                "the judges disagree or gave no verdict on {:.0}% of the drafts",
                share * 100.0
            ));
        }
    }
    out
}

/// A fixed, platform-independent hash for the display order.
pub fn order_hash(id: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Evenly spaced picks over the score-sorted list (so a cell's reads are
/// representative), preferring a case not yet picked.
fn base_picks<'a>(sorted: &[(&'a Draft, usize)], k: usize) -> Vec<&'a Draft> {
    let n = sorted.len();
    if n <= k {
        return sorted.iter().map(|(d, _)| *d).collect();
    }
    let mut picked: Vec<&Draft> = Vec::new();
    let mut seen_cases: BTreeSet<&str> = BTreeSet::new();
    for i in 0..k {
        let target = if k == 1 { 0 } else { i * (n - 1) / (k - 1) };
        // Nearest unpicked position, preferring a new case.
        let mut best: Option<usize> = None;
        for dist in 0..n {
            for cand in [target.checked_sub(dist), Some(target + dist)]
                .into_iter()
                .flatten()
            {
                if cand >= n || picked.iter().any(|p| p.id == sorted[cand].0.id) {
                    continue;
                }
                let fresh = !seen_cases.contains(sorted[cand].0.case.as_str());
                if fresh {
                    best = Some(cand);
                    break;
                }
                if best.is_none() {
                    best = Some(cand);
                }
            }
            if best.is_some_and(|b| !seen_cases.contains(sorted[b].0.case.as_str())) {
                break;
            }
        }
        let b = best.expect("an unpicked draft exists");
        seen_cases.insert(sorted[b].0.case.as_str());
        picked.push(sorted[b].0);
    }
    picked
}

pub fn select(
    run: &str,
    drafts: &[Draft],
    verdicts: &BTreeMap<String, Vec<JudgeRecord>>,
    tags: &[String],
    cfg: &Config,
) -> Manifest {
    // No saved verdict anywhere means the judge was not run: judging adds nothing to
    // the choice, and a missing verdict is not a disagreement.
    let tags: &[String] = if verdicts.is_empty() { &[] } else { tags };
    // Cells in a stable order.
    let mut cells: BTreeMap<(String, String), Vec<&Draft>> = BTreeMap::new();
    for d in drafts.iter().filter(|d| !d.errored()) {
        cells
            .entry((d.group.clone(), d.rec.model.clone()))
            .or_default()
            .push(d);
    }
    let cell_sizes: BTreeMap<String, usize> = cells
        .iter()
        .map(|((g, m), v)| (cell_key(g, m), v.len()))
        .collect();
    // Priority: borderline cells first, then the rest, each in key order.
    let mut order: Vec<((String, String), Vec<String>)> = cells
        .iter()
        .map(|(k, v)| (k.clone(), borderline_reasons(v, verdicts, tags, cfg)))
        .collect();
    order.sort_by_key(|(_, r)| r.is_empty());

    let mut entries: Vec<Entry> = Vec::new();
    let mut unreached: Vec<String> = Vec::new();
    let mut per_cell: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut sorted_cells: BTreeMap<(String, String), Vec<(&Draft, usize)>> = BTreeMap::new();
    for (key, items) in &cells {
        let mut s: Vec<(&Draft, usize)> = items
            .iter()
            .map(|d| {
                let vs = verdicts.get(&d.id).map_or(&[][..], Vec::as_slice);
                (*d, item_score(d, vs, expected_judges(d, tags)))
            })
            .collect();
        s.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        sorted_cells.insert(key.clone(), s);
    }

    // Base reads.
    for (key, reasons) in &order {
        let sorted = &sorted_cells[key];
        let k = cfg.reads.min_per_cell.min(sorted.len());
        if entries.len() + k > cfg.reads.max_total {
            unreached.push(cell_key(&key.0, &key.1));
            continue;
        }
        for d in base_picks(sorted, k) {
            entries.push(Entry {
                id: d.id.clone(),
                group: key.0.clone(),
                model: key.1.clone(),
                reasons: vec![if reasons.is_empty() {
                    "base read".to_owned()
                } else {
                    "base read of a borderline cell".to_owned()
                }],
            });
        }
        per_cell.insert(key.clone(), k);
    }
    // Extra reads for borderline cells that got their base reads, most informative first.
    for (key, reasons) in &order {
        if reasons.is_empty() || !per_cell.contains_key(key) {
            continue;
        }
        let have: BTreeSet<String> = entries.iter().map(|e| e.id.clone()).collect();
        for (d, score) in &sorted_cells[key] {
            if entries.len() >= cfg.reads.max_total || per_cell[key] >= cfg.reads.max_per_cell {
                break;
            }
            if have.contains(&d.id) || *score == 0 {
                continue;
            }
            entries.push(Entry {
                id: d.id.clone(),
                group: key.0.clone(),
                model: key.1.clone(),
                reasons: reasons.clone(),
            });
            *per_cell.get_mut(key).expect("cell") += 1;
        }
    }
    Manifest {
        run: run.to_owned(),
        max_total: cfg.reads.max_total,
        entries,
        unreached,
        cell_sizes,
    }
}

// ---------------------------------------------------------------------------
// read.html
// ---------------------------------------------------------------------------

fn esc_attr(s: &str) -> String {
    esc(s).replace('"', "&quot;")
}

fn group_label(group: &str) -> String {
    match group {
        GROUP_APPLY => "Voice: rewrite a draft in a voice (quick_response)".to_owned(),
        GROUP_REFINE => "Voice: write guidance for a voice (quick_response)".to_owned(),
        t => format!("Writing: {t}"),
    }
}

fn findings_html(d: &Draft) -> String {
    let r = &d.report;
    let mut parts: Vec<String> = Vec::new();
    if !r.placeholders.is_empty() {
        parts.push(format!("placeholders: {}", r.placeholders.join(", ")));
    }
    if let Some(o) = &r.opening_chatter {
        parts.push(format!("opening remark: {o}"));
    }
    if let Some(c) = &r.closing_chatter {
        parts.push(format!("closing remark: {c}"));
    }
    if !r.unsupported.is_empty() {
        parts.push(format!("not in the prompt: {}", r.unsupported.join(", ")));
    }
    if !r.missing_points.is_empty() {
        parts.push(format!(
            "points not found: {}",
            r.missing_points.join(" / ")
        ));
    }
    if parts.is_empty() {
        "no deterministic findings".to_owned()
    } else {
        parts.join(" | ")
    }
}

pub fn read_html(
    manifest: &Manifest,
    drafts: &[Draft],
    verdicts: &BTreeMap<String, Vec<JudgeRecord>>,
) -> String {
    let by_id: BTreeMap<&str, &Draft> = drafts.iter().map(|d| (d.id.as_str(), d)).collect();
    let mut entries: Vec<&Entry> = manifest.entries.iter().collect();
    entries.sort_by_key(|e| order_hash(&e.id));
    let n = entries.len();
    let cell_count = manifest
        .entries
        .iter()
        .map(|e| cell_key(&e.group, &e.model))
        .collect::<BTreeSet<_>>()
        .len();
    let mut s = String::new();
    s.push_str(&format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
<title>Read: {run}</title>\
<style>:root{{--bg:#fff;--fg:#1a1a1a;--mut:#666;--line:#ccc;--acc:#1b5fb5;--card:#f6f6f6}}\
@media(prefers-color-scheme:dark){{:root{{--bg:#16181b;--fg:#e6e6e6;--mut:#9aa0a6;--line:#3a3d42;--acc:#6aa5f0;--card:#1f2226}}}}\
body{{background:var(--bg);color:var(--fg);font-family:system-ui,sans-serif;margin:0 auto;max-width:56rem;padding:1rem 16px 5rem}}\
section{{border:1px solid var(--line);border-radius:6px;padding:.75rem 1rem;margin:1rem 0;background:var(--card)}}\
pre{{white-space:pre-wrap;word-break:break-word;font:inherit;background:var(--bg);border:1px solid var(--line);padding:.6rem;border-radius:4px}}\
.mut{{color:var(--mut);font-size:.85rem}}label{{margin-right:1rem;white-space:nowrap}}\
input[type=text]{{width:100%;box-sizing:border-box;margin-top:.5rem;padding:.3rem;background:var(--bg);color:var(--fg);border:1px solid var(--line)}}\
.bar{{position:sticky;top:0;background:var(--bg);padding:.5rem 0;border-bottom:1px solid var(--line)}}\
button{{padding:.4rem .8rem}}</style>\
<h1>Read: {run}</h1>\
<p><strong>{n} drafts to read</strong> (limit {cap}), covering {cells} cells. {unreached} cell(s) not reached by the limit stay unmeasured{unreached_names}.</p>\
<p class=mut>Give each draft the highest bar it reaches: <b>below_basic</b> (not usable), <b>basic</b> (usable after a few fixes), <b>standard</b> (usable with light edits), <b>high</b> (usable as it is). \
The writer's model is hidden and the order is shuffled. The judge's verdicts are folded away on purpose: open them after you rate if you want to compare. \
Your ratings are saved in this browser as you go. When you are done, press Export and keep ratings.json.</p>\
<div class=bar><span id=progress>0 of {n} rated</span> <button id=export type=button>Export ratings.json</button></div>",
        run = esc(&manifest.run),
        n = n,
        cap = manifest.max_total,
        cells = cell_count,
        unreached = manifest.unreached.len(),
        unreached_names = if manifest.unreached.is_empty() {
            String::new()
        } else {
            format!(" ({})", esc(&manifest.unreached.join(", ")))
        },
    ));
    for (i, e) in entries.iter().enumerate() {
        let Some(d) = by_id.get(e.id.as_str()) else {
            continue;
        };
        let judge_html = match verdicts.get(&d.id) {
            None => "no judge verdict".to_owned(),
            Some(vs) => vs
                .iter()
                .map(|v| match &v.verdict {
                    Some(x) => format!(
                        "judge {}: <b>{}</b> ({})",
                        esc(&v.judge),
                        esc(&x.bar),
                        esc(&x
                            .criteria
                            .iter()
                            .map(|(k, b)| format!("{k}={b}"))
                            .collect::<Vec<_>>()
                            .join(", "))
                    ),
                    None => format!("judge {}: no verdict", esc(&v.judge)),
                })
                .collect::<Vec<_>>()
                .join("<br>"),
        };
        s.push_str(&format!(
            "<section data-id=\"{id}\"><h3>Draft {num} of {n}: {label}</h3>\
<details><summary>The instructions the writer was given</summary><pre>{instr}</pre></details>\
<pre>{text}</pre>\
<p class=mut>{findings}</p>\
<details><summary>Judge verdicts (open after you rate)</summary><p class=mut>{judge}</p></details>\
<div role=radiogroup>{radios}</div>\
<input type=text placeholder=\"Note (optional)\" aria-label=\"Note\"></section>",
            id = esc_attr(&e.id),
            num = i + 1,
            n = n,
            label = esc(&group_label(&e.group)),
            instr = esc(&d.instructions),
            text = esc(&d.text),
            findings = esc(&findings_html(d)),
            judge = judge_html,
            radios = ["below_basic", "basic", "standard", "high"]
                .iter()
                .map(|b| format!(
                    "<label><input type=radio name=\"r{i}\" value=\"{b}\"> {b}</label>"
                ))
                .collect::<String>(),
        ));
    }
    s.push_str(
        "<script>(function(){var KEY='qr899-read-'+document.title;var st={};\
try{st=JSON.parse(localStorage.getItem(KEY)||'{}')}catch(e){st={}}\
function save(){try{localStorage.setItem(KEY,JSON.stringify(st))}catch(e){}}\
var secs=document.querySelectorAll('section[data-id]');\
function prog(){var n=0;for(var k in st){if(st[k]&&st[k].bar)n++}document.getElementById('progress').textContent=n+' of '+secs.length+' rated'}\
secs.forEach(function(sec){var id=sec.getAttribute('data-id');var cur=st[id]||{};\
sec.querySelectorAll('input[type=radio]').forEach(function(r){if(cur.bar===r.value)r.checked=true;\
r.addEventListener('change',function(){st[id]=st[id]||{};st[id].bar=r.value;save();prog()})});\
var note=sec.querySelector('input[type=text]');if(cur.note)note.value=cur.note;\
note.addEventListener('input',function(){st[id]=st[id]||{};st[id].note=note.value;save()})});\
document.getElementById('export').addEventListener('click',function(){var reads=[];\
secs.forEach(function(sec){var id=sec.getAttribute('data-id');var v=st[id];if(v&&v.bar)reads.push({id:id,bar:v.bar,note:v.note||''})});\
var blob=new Blob([JSON.stringify({run:document.title.replace('Read: ',''),exported_at:new Date().toISOString(),reads:reads},null,1)],{type:'application/json'});\
var a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download='ratings.json';document.body.appendChild(a);a.click();a.remove()});\
prog()})();</script>",
    );
    s
}

// ---------------------------------------------------------------------------
// Ratings, cells, agreement, emit
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Read {
    pub id: String,
    pub bar: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Deserialize)]
struct RatingsFile {
    run: String,
    reads: Vec<Read>,
}

/// Checks a ratings.json against the manifest: same run, every bar in the
/// vocabulary, every id a manifest entry, no id twice.
pub fn parse_ratings(text: &str, manifest: &Manifest, cfg: &Config) -> Result<Vec<Read>, String> {
    let f: RatingsFile = serde_json::from_str(text).map_err(|e| format!("ratings.json: {e}"))?;
    if f.run != manifest.run {
        return Err(format!(
            "ratings.json is for run '{}', not '{}'",
            f.run, manifest.run
        ));
    }
    let ids: BTreeSet<&str> = manifest.entries.iter().map(|e| e.id.as_str()).collect();
    let mut seen = BTreeSet::new();
    for r in &f.reads {
        if !cfg.vocabulary.contains(&r.bar) {
            return Err(format!("{}: '{}' is not a bar", r.id, r.bar));
        }
        if !ids.contains(r.id.as_str()) {
            return Err(format!("{}: not a draft of this read page", r.id));
        }
        if !seen.insert(r.id.clone()) {
            return Err(format!("{}: rated twice", r.id));
        }
    }
    Ok(f.reads)
}

/// The highest bar that at least `share` of the reads reach: sort the reads from
/// the best down and take the one at position ceil(share * n) (a tiny epsilon
/// keeps 0.66 * 3 at two reads, not three).
pub fn rollup(bars: &[&str], share: f64) -> Option<&'static str> {
    let mut ranks: Vec<usize> = bars.iter().filter_map(|b| bar_rank(b)).collect();
    if ranks.is_empty() {
        return None;
    }
    ranks.sort_unstable_by(|a, b| b.cmp(a));
    let need = ((share * ranks.len() as f64) - 1e-9).ceil().max(1.0) as usize;
    ["below_basic", "basic", "standard", "high"]
        .get(ranks[need - 1])
        .copied()
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct CellOut {
    pub task_type: String,
    /// A shipped document type, or empty for a task-type-wide cell.
    pub profile: String,
    pub model: String,
    pub bar: String,
    pub model_class: String,
    pub sample_count: usize,
    pub reads: usize,
    pub note: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Unmeasured {
    pub task_type: String,
    pub profile: String,
    pub model: String,
    pub reason: String,
}

fn counts_text(bars: &[&str]) -> String {
    ["high", "standard", "basic", "below_basic"]
        .iter()
        .filter_map(|b| {
            let n = bars.iter().filter(|x| *x == b).count();
            (n > 0).then(|| format!("{n} {b}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn build_cells(
    reads: &[Read],
    manifest: &Manifest,
    cfg: &Config,
) -> Result<(Vec<CellOut>, Vec<Unmeasured>), String> {
    let by_id: BTreeMap<&str, &Entry> = manifest
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let mut by_cell: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for r in reads {
        let e = by_id
            .get(r.id.as_str())
            .ok_or_else(|| format!("{}: not in the manifest", r.id))?;
        by_cell
            .entry((e.group.clone(), e.model.clone()))
            .or_default()
            .push(r.bar.as_str());
    }
    let mut cells = Vec::new();
    let mut unmeasured = Vec::new();
    let class_of = |m: &str| -> Result<String, String> {
        cfg.model_class
            .get(m)
            .cloned()
            .ok_or_else(|| format!("bars.json has no model_class for {m}"))
    };
    let size_of = |g: &str, m: &str| {
        manifest
            .cell_sizes
            .get(&cell_key(g, m))
            .copied()
            .unwrap_or(0)
    };
    let models: BTreeSet<&str> = manifest
        .cell_sizes
        .keys()
        .filter_map(|k| k.split_once('|').map(|x| x.1))
        .collect();
    let groups: BTreeSet<&str> = manifest
        .cell_sizes
        .keys()
        .filter_map(|k| k.split_once('|').map(|x| x.0))
        .collect();

    // Writing cells: one per (document type, model).
    for g in groups
        .iter()
        .filter(|g| **g != GROUP_APPLY && **g != GROUP_REFINE)
    {
        for m in &models {
            if !manifest.cell_sizes.contains_key(&cell_key(g, m)) {
                continue;
            }
            let bars = by_cell
                .get(&(g.to_string(), m.to_string()))
                .cloned()
                .unwrap_or_default();
            if bars.len() < cfg.reads.min_per_cell {
                unmeasured.push(Unmeasured {
                    task_type: "creative_writing".to_owned(),
                    profile: g.to_string(),
                    model: m.to_string(),
                    reason: format!("{} reads, {} needed", bars.len(), cfg.reads.min_per_cell),
                });
                continue;
            }
            let bar = rollup(&bars, cfg.rollup.share_at_or_above).expect("a bar");
            cells.push(CellOut {
                task_type: "creative_writing".to_owned(),
                profile: g.to_string(),
                model: m.to_string(),
                bar: bar.to_owned(),
                model_class: class_of(m)?,
                sample_count: size_of(g, m),
                reads: bars.len(),
                note: format!(
                    "Read {} of {} drafts: {}.",
                    bars.len(),
                    size_of(g, m),
                    counts_text(&bars)
                ),
            });
        }
    }
    // quick_response: one task-type-wide cell per model, the lower of the two modes.
    for m in &models {
        let have_voice = [GROUP_APPLY, GROUP_REFINE]
            .iter()
            .any(|g| manifest.cell_sizes.contains_key(&cell_key(g, m)));
        if !have_voice {
            continue;
        }
        let mut mode_bars: Vec<(&str, &'static str, usize)> = Vec::new();
        let mut short: Option<String> = None;
        for g in [GROUP_APPLY, GROUP_REFINE] {
            let bars = by_cell
                .get(&(g.to_owned(), m.to_string()))
                .cloned()
                .unwrap_or_default();
            if bars.len() < cfg.reads.min_per_cell {
                short = Some(format!(
                    "{g}: {} reads, {} needed",
                    bars.len(),
                    cfg.reads.min_per_cell
                ));
                break;
            }
            mode_bars.push((
                g,
                rollup(&bars, cfg.rollup.share_at_or_above).expect("a bar"),
                bars.len(),
            ));
        }
        if let Some(reason) = short {
            unmeasured.push(Unmeasured {
                task_type: "quick_response".to_owned(),
                profile: String::new(),
                model: m.to_string(),
                reason,
            });
            continue;
        }
        let low = mode_bars
            .iter()
            .map(|(_, b, _)| *b)
            .min_by_key(|b| bar_rank(b))
            .expect("two modes");
        let total_reads: usize = mode_bars.iter().map(|x| x.2).sum();
        let size = size_of(GROUP_APPLY, m) + size_of(GROUP_REFINE, m);
        cells.push(CellOut {
            task_type: "quick_response".to_owned(),
            profile: String::new(),
            model: m.to_string(),
            bar: low.to_owned(),
            model_class: class_of(m)?,
            sample_count: size,
            reads: total_reads,
            note: format!(
                "Voice step, both modes, the lower bar. Read {} of {} drafts: apply {}, refine {}.",
                total_reads, size, mode_bars[0].1, mode_bars[1].1
            ),
        });
    }
    Ok((cells, unmeasured))
}

#[derive(Serialize, Default, Clone, Debug, PartialEq)]
pub struct Agreement {
    pub judge: String,
    pub compared: usize,
    pub exact: usize,
    pub within_one: usize,
    pub judge_higher: usize,
    pub judge_lower: usize,
}

pub fn agreement(reads: &[Read], verdicts: &BTreeMap<String, Vec<JudgeRecord>>) -> Vec<Agreement> {
    let mut by_judge: BTreeMap<String, Agreement> = BTreeMap::new();
    for r in reads {
        let Some(jr) = bar_rank(&r.bar) else { continue };
        for v in verdicts.get(&r.id).map_or(&[][..], Vec::as_slice) {
            let Some(x) = &v.verdict else { continue };
            let Some(jb) = bar_rank(&x.bar) else { continue };
            let a = by_judge
                .entry(v.judge.clone())
                .or_insert_with(|| Agreement {
                    judge: v.judge.clone(),
                    ..Agreement::default()
                });
            a.compared += 1;
            if jb == jr {
                a.exact += 1;
            }
            if jb.abs_diff(jr) <= 1 {
                a.within_one += 1;
            }
            if jb > jr {
                a.judge_higher += 1;
            }
            if jb < jr {
                a.judge_lower += 1;
            }
        }
    }
    by_judge.into_values().collect()
}

pub fn agreement_md(a: &[Agreement], n_reads: usize) -> String {
    let mut s = format!(
        "# Judge against Jason's reads\n\n{n_reads} reads. A judge's bar is never written to a cell; this table says how far to trust it for choosing drafts.\n\n\
| judge | compared | same bar | within one step | judge higher | judge lower |\n|---|---|---|---|---|---|\n"
    );
    for x in a {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            x.judge, x.compared, x.exact, x.within_one, x.judge_higher, x.judge_lower
        ));
    }
    if a.is_empty() {
        s.push_str("| (no judge verdicts were saved for the rated drafts) | | | | | |\n");
    }
    s
}

fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The VALUES rows for shared_045.sql, in its layout. A note never carries a
/// semicolon (the schema authoring rule).
pub fn emit_sql(cells: &[CellOut], date: &str) -> String {
    let rows: Vec<String> = cells
        .iter()
        .map(|c| {
            assert!(!c.note.contains(';'), "a note may not contain a semicolon");
            format!(
                "    ('local', '', {}, {}, {}, {},\n     {}, {}, {}, 'jason_read',\n     {})",
                sql_quote(&c.model),
                sql_quote(&c.task_type),
                sql_quote(&c.profile),
                sql_quote(&c.bar),
                sql_quote(&c.model_class),
                c.sample_count,
                sql_quote(date),
                sql_quote(&c.note)
            )
        })
        .collect();
    rows.join(",\n")
}

/// The catalog `capabilities` array (provider_catalog::format::CapabilityDoc).
pub fn emit_catalog(cells: &[CellOut], date: &str) -> Value {
    Value::Array(
        cells
            .iter()
            .map(|c| {
                let mut o = json!({
                    "subject": "local",
                    "model_id": c.model,
                    "task_type": c.task_type,
                    "attained_bar": c.bar,
                    "model_class": c.model_class,
                    "sample_count": c.sample_count,
                    "measured_at": date,
                    "provenance": "jason_read",
                    "note": c.note,
                });
                if !c.profile.is_empty() {
                    o["profile"] = json!(c.profile);
                }
                o
            })
            .collect(),
    )
}

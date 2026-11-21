// src-tauri/src/conductor/check_block.rs
//
// items.id=903 (decisions.id=942 point 3, decisions.id=963 rulings 2 and 3): the
// DRAFT CHECK of cb-06, the optional `draft_check` section of a cb-06 row. Pure: no
// I/O, no provider, no model call, no write (a source scan pins it). cb-06's handler
// (conductor::block_dispatch) parses the section and calls evaluate().
//
// WHAT IT DOES. It runs conductor::draft_check::check over the draft with the text
// of the row's SOURCES as the allowed text, then decide() with the latitude in force
// and the row's attempt number, and adds three measures of its own:
//   * coverage: the bullet points ("- " lines) of each source, a point covered when
//     at least half of its significant words start a word of the draft, weighted per
//     source (weight 0 keeps a source as allowed text only). A measure, never a
//     finding: it never changes the verdict;
//   * a section check (section_var): the draft addresses its section's title and
//     writes no heading of another section. Marks, never regenerates;
//   * a voice check (voice_check): no stock phrase of the data's suppress list, and an
//     average sentence no longer than the pacing in force allows. Marks only.
// A source that is not available (an absent artifact) is noted and MARKS the draft:
// a check that could not see its brief never accepts it and never regenerates it.
//
// THE ROW RECORDS ONE OF TWO THINGS (emit):
//   verdict  exactly one of VERDICT_LITERALS, so a later row can be guarded on it
//            (the guarded second draft row runs on `regenerate`, at most once: the
//            check after it has attempt 1, which never regenerates);
//   report   a content-free summary: categories and counts only, never a word of
//            the draft or a source (cb-06's verdict-text rule).
//
// WHICH LATITUDE. A run start fills `run` (run_settings::inject_check_settings) with
// the run's selected document type, its resolved latitude and pacing, from the same
// resolution the model blocks are re-expanded with. Without a run start (LOAD's own
// view, a test) the row's document_type supplies its shipped defaults. An author may
// not write `run` (LOAD refuses it).
//
// WHAT IT MAY READ (decisions.id=963 ruling 3, amending decisions.id=942's pure-block
// contract): the row's DECLARED names only, read-only. declared() is what the loader
// puts in the row's declared_inputs, so the declared-inputs lint checks each is real
// (an earlier row's output or a bound name), seeding reads the bound ones at pass
// start, and execute_block_step hands the handler only those seeds.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::conductor::draft_check::{check, decide, CheckInput, CheckReport, Verdict};
use crate::conductor::model_block::config::is_slug;
use crate::conductor::model_block::document_type::{registry, Latitude, Registry};

/// The values a verdict row records, in the order a reader expects.
pub const VERDICT_LITERALS: [&str; 4] = ["accept", "regenerate", "mark", "skipped"];

/// Most sources one check may name.
pub const MAX_SOURCES: usize = 6;

/// What the row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Emit {
    Verdict,
    Report,
}

/// One source of allowed text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSource {
    /// An earlier row's output variable or a bound name (content_brief).
    pub var: String,
    /// Coverage weight. Defaults to 1.0; 0.0 keeps the source as allowed text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
}

/// What the run start fills in. Never authored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCheckSettings {
    pub document_type: String,
    pub latitude: Latitude,
    pub pacing: String,
}

/// The `draft_check` section of a cb-06 row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftCheckConfig {
    /// The document type whose shipped latitude, pacing and citation setting apply
    /// until a run start supplies the run's own.
    pub document_type: String,
    pub emit: Emit,
    /// 0 for the first draft, 1 for the draft after a regeneration.
    pub attempt: u32,
    pub sources: Vec<CheckSource>,
    /// The variable holding the section being drafted (section passes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_var: Option<String>,
    #[serde(default)]
    pub voice_check: bool,
    /// Filled by the run start. LOAD refuses an authored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunCheckSettings>,
}

impl DraftCheckConfig {
    /// `authored`: the row as written in the Focus data (LOAD), which may not carry
    /// `run`. The handler validates with false, after a run start filled it.
    pub fn validate(&self, authored: bool) -> Result<(), String> {
        let reg = registry()?;
        if reg.document_type(&self.document_type).is_none() {
            return Err(format!(
                "draft_check: unknown document_type '{}'",
                self.document_type
            ));
        }
        if self.attempt > 1 {
            return Err(
                "draft_check: attempt is 0 (first draft) or 1 (after a regeneration)".to_owned(),
            );
        }
        if self.sources.is_empty() || self.sources.len() > MAX_SOURCES {
            return Err(format!(
                "draft_check: between 1 and {MAX_SOURCES} sources are needed"
            ));
        }
        for (i, s) in self.sources.iter().enumerate() {
            if !is_slug(&s.var) {
                return Err(format!(
                    "draft_check: source '{}' must be a lower-case name",
                    s.var
                ));
            }
            if self.sources[..i].iter().any(|o| o.var == s.var) {
                return Err(format!("draft_check: source '{}' is listed twice", s.var));
            }
            if let Some(w) = s.weight {
                if !w.is_finite() || !(0.0..=10.0).contains(&w) {
                    return Err(format!(
                        "draft_check: source '{}' weight must be between 0 and 10",
                        s.var
                    ));
                }
            }
        }
        if let Some(v) = &self.section_var {
            if !is_slug(v) {
                return Err(format!(
                    "draft_check: section_var '{v}' must be a lower-case name"
                ));
            }
        }
        match (&self.run, authored) {
            (Some(_), true) => {
                return Err(
                    "draft_check: run is filled by the run start and may not be authored"
                        .to_owned(),
                )
            }
            (Some(r), false) => {
                if reg.document_type(&r.document_type).is_none()
                    || reg.scales.pacing(&r.pacing).is_none()
                {
                    return Err("draft_check: the run settings are not valid".to_owned());
                }
            }
            (None, _) => {}
        }
        Ok(())
    }

    /// The names the row reads besides its draft: the loader's declared_inputs.
    pub fn declared(&self) -> Vec<String> {
        let mut out: Vec<String> = self.sources.iter().map(|s| s.var.clone()).collect();
        if let Some(v) = &self.section_var {
            if !out.contains(v) {
                out.push(v.clone());
            }
        }
        out
    }
}

/// What a section check found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SectionFindings {
    /// Fewer than half of the section title's significant words are in the draft.
    pub off_topic: bool,
    /// Headings in the draft that are not the section's own.
    pub extra_headings: usize,
}

/// What a voice check found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceFindings {
    pub stock_phrases: usize,
    pub sentences_too_long: bool,
}

/// Coverage of the sources' points.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Coverage {
    pub covered: usize,
    pub total: usize,
    /// The per-source fractions weighted by the sources' weights (1.0 with no
    /// weighted points).
    pub weighted: f32,
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub verdict: Verdict,
    pub report: CheckReport,
    pub coverage: Coverage,
    pub section: Option<SectionFindings>,
    pub voice: Option<VoiceFindings>,
    /// Sources that were not available, by NAME.
    pub missing_sources: Vec<String>,
    pub draft_missing: bool,
}

impl Outcome {
    /// The literal a verdict row records.
    pub fn verdict_literal(&self) -> &'static str {
        match self.verdict {
            Verdict::Accept => VERDICT_LITERALS[0],
            Verdict::Regenerate => VERDICT_LITERALS[1],
            Verdict::Mark => VERDICT_LITERALS[2],
            Verdict::Skipped => VERDICT_LITERALS[3],
        }
    }

    /// The content-free summary a report row records: categories and counts and
    /// source NAMES only.
    pub fn report_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let count =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        if self.draft_missing {
            parts.push("the draft was not available".to_owned());
        }
        for s in &self.missing_sources {
            parts.push(format!("source '{s}' was not available"));
        }
        if self.verdict != Verdict::Skipped {
            let r = &self.report;
            if !r.unsupported.is_empty() {
                parts.push(count(
                    r.unsupported.len(),
                    "unsupported specific",
                    "unsupported specifics",
                ));
            }
            if !r.placeholders.is_empty() {
                parts.push(count(r.placeholders.len(), "placeholder", "placeholders"));
            }
            if r.opening_chatter.is_some() {
                parts.push("an opening remark".to_owned());
            }
            if r.closing_chatter.is_some() {
                parts.push("a closing remark".to_owned());
            }
            if !r.unsupplied_references.is_empty() {
                parts.push(count(
                    r.unsupplied_references.len(),
                    "unsupplied reference",
                    "unsupplied references",
                ));
            }
            if let Some(s) = &self.section {
                if s.off_topic {
                    parts.push("the section title is not addressed".to_owned());
                }
                if s.extra_headings > 0 {
                    parts.push(count(
                        s.extra_headings,
                        "heading beyond the section",
                        "headings beyond the section",
                    ));
                }
            }
            if let Some(v) = &self.voice {
                if v.stock_phrases > 0 {
                    parts.push(count(v.stock_phrases, "stock phrase", "stock phrases"));
                }
                if v.sentences_too_long {
                    parts.push("sentences longer than the pacing allows".to_owned());
                }
            }
            if self.coverage.total > 0 {
                parts.push(format!(
                    "{} of {} brief points covered (weighted {:.2})",
                    self.coverage.covered, self.coverage.total, self.coverage.weighted
                ));
            }
        }
        let mut text = format!("Draft check: {}.", self.verdict_literal());
        if !parts.is_empty() {
            text.push(' ');
            text.push_str(&parts.join("; "));
            text.push('.');
        }
        text
    }
}

/// The latitude, pacing and citation setting in force.
fn in_force<'r>(
    cfg: &DraftCheckConfig,
    reg: &'r Registry,
) -> Result<
    (
        Latitude,
        String,
        &'r crate::conductor::model_block::document_type::DocumentType,
    ),
    String,
> {
    match &cfg.run {
        Some(r) => {
            let t = reg
                .document_type(&r.document_type)
                .ok_or("draft_check: the run's document type is not known")?;
            Ok((r.latitude, r.pacing.clone(), t))
        }
        None => {
            let t = reg
                .document_type(&cfg.document_type)
                .ok_or("draft_check: unknown document_type")?;
            Ok((t.settings.latitude, t.settings.pacing.clone(), t))
        }
    }
}

const STOP: &[&str] = &[
    "about", "after", "also", "been", "before", "from", "have", "into", "just", "more", "only",
    "over", "that", "their", "them", "then", "there", "they", "this", "were", "what", "when",
    "which", "while", "will", "with", "would", "your",
];

/// Lower-case words of at least four characters that are not stop words.
fn significant(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !STOP.contains(w))
        .map(str::to_owned)
        .collect()
}

/// Every word of the draft, lower case.
fn draft_words(draft: &str) -> std::collections::HashSet<String> {
    draft
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// At least half (rounded up) of `words` are words of the draft. An empty list
/// counts as covered.
fn mostly_present(words: &[String], draft: &std::collections::HashSet<String>) -> bool {
    let hits = words.iter().filter(|w| draft.contains(*w)).count();
    hits * 2 >= words.len()
}

fn coverage(
    cfg: &DraftCheckConfig,
    value: &dyn Fn(&str) -> Option<String>,
    words: &std::collections::HashSet<String>,
) -> Coverage {
    let (mut covered, mut total) = (0usize, 0usize);
    let (mut sum, mut weight_sum) = (0f32, 0f32);
    for s in &cfg.sources {
        let Some(text) = value(&s.var) else { continue };
        let points: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("- "))
            .collect();
        if points.is_empty() {
            continue;
        }
        let hit = points
            .iter()
            .filter(|p| mostly_present(&significant(p), words))
            .count();
        covered += hit;
        total += points.len();
        let w = s.weight.unwrap_or(1.0);
        if w > 0.0 {
            sum += w * hit as f32 / points.len() as f32;
            weight_sum += w;
        }
    }
    Coverage {
        covered,
        total,
        weighted: if weight_sum > 0.0 {
            sum / weight_sum
        } else {
            1.0
        },
    }
}

/// The section's title: its first non-empty line, without heading or bullet marks.
fn section_title(section: &str) -> String {
    section
        .lines()
        .map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn section_findings(section: &str, draft: &str) -> SectionFindings {
    let title = section_title(section);
    let words = draft_words(draft);
    let title_words = significant(&title);
    let title_low = title.to_lowercase();
    let extra_headings = draft
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_lowercase())
        .filter(|h| !h.is_empty() && *h != title_low)
        .count();
    SectionFindings {
        off_topic: !mostly_present(&title_words, &words),
        extra_headings,
    }
}

fn voice_findings(draft: &str, pacing: &str, reg: &Registry) -> VoiceFindings {
    let low = draft.to_lowercase().replace('\u{2019}', "'");
    let stock_phrases = reg
        .check
        .suppress_phrases
        .iter()
        .filter(|p| low.contains(p.as_str()))
        .count();
    let sentences: Vec<usize> = draft
        .split(['.', '!', '?'])
        .map(|s| s.split_whitespace().count())
        .filter(|&n| n > 0)
        .collect();
    let max = reg.check.sentence_words.get(pacing).copied();
    let sentences_too_long = match (max, sentences.is_empty()) {
        (Some(max), false) => {
            let avg = sentences.iter().sum::<usize>() as f32 / sentences.len() as f32;
            avg > max as f32
        }
        _ => false,
    };
    VoiceFindings {
        stock_phrases,
        sentences_too_long,
    }
}

/// Run the check. `draft` is the input variable's text (None when it was not
/// produced); `value` returns a declared name's text, or None when it is absent.
pub fn evaluate(
    cfg: &DraftCheckConfig,
    draft: Option<&str>,
    value: &dyn Fn(&str) -> Option<String>,
) -> Result<Outcome, String> {
    let reg = registry()?;
    let (latitude, pacing, doc_type) = in_force(cfg, reg)?;
    let missing_sources: Vec<String> = cfg
        .sources
        .iter()
        .filter(|s| value(&s.var).is_none())
        .map(|s| s.var.clone())
        .collect();
    let Some(draft) = draft else {
        return Ok(Outcome {
            verdict: Verdict::Skipped,
            report: CheckReport::default(),
            coverage: Coverage::default(),
            section: None,
            voice: None,
            missing_sources,
            draft_missing: true,
        });
    };
    let allowed: String = cfg
        .sources
        .iter()
        .filter_map(|s| value(&s.var))
        .collect::<Vec<_>>()
        .join("\n");
    let report = check(
        draft,
        &CheckInput {
            allowed_text: &allowed,
            points: &[],
            citations: doc_type.citation_markers,
        },
    );
    let words = draft_words(draft);
    let coverage = coverage(cfg, value, &words);
    let section = cfg
        .section_var
        .as_ref()
        .map(|v| section_findings(&value(v).unwrap_or_default(), draft));
    let voice = cfg.voice_check.then(|| voice_findings(draft, &pacing, reg));

    let mut verdict = decide(&report, latitude, cfg.attempt);
    let marks = !missing_sources.is_empty()
        || section
            .as_ref()
            .is_some_and(|s| s.off_topic || s.extra_headings > 0)
        || voice
            .as_ref()
            .is_some_and(|v| v.stock_phrases > 0 || v.sentences_too_long);
    // A missing source never lets a draft be accepted OR regenerated: the check could
    // not see what the draft was meant to draw on.
    if !missing_sources.is_empty() && verdict == Verdict::Regenerate {
        verdict = Verdict::Mark;
    }
    if marks && verdict == Verdict::Accept {
        verdict = Verdict::Mark;
    }
    Ok(Outcome {
        verdict,
        report,
        coverage,
        section,
        voice,
        missing_sources,
        draft_missing: false,
    })
}

/// items.id=903: what a run start writes into a cb-06 row's draft_check section.
pub fn with_run_settings(
    customization: &serde_json::Value,
    run: RunCheckSettings,
) -> Result<Option<serde_json::Value>, String> {
    let Some(section) = customization.get("draft_check") else {
        return Ok(None);
    };
    let mut cfg: DraftCheckConfig =
        serde_json::from_value(section.clone()).map_err(|e| format!("draft_check: {e}"))?;
    cfg.run = Some(run);
    cfg.validate(false)?;
    let mut out = customization.clone();
    out["draft_check"] = serde_json::to_value(&cfg).map_err(|e| e.to_string())?;
    Ok(Some(out))
}

/// A lookup over the declared values the handler was given.
pub fn lookup<'a>(values: &'a HashMap<String, String>) -> impl Fn(&str) -> Option<String> + 'a {
    move |k: &str| values.get(k).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: serde_json::Value) -> DraftCheckConfig {
        let c: DraftCheckConfig = serde_json::from_value(v).expect("parses");
        c.validate(true).expect("valid");
        c
    }

    fn brief() -> String {
        "Task: Ask for a refund\nRecipient: Returns Department\n\nPoints:\n\
         - The drill stopped working after two days\n\
         - Bought on 2 November at the Leeds store"
            .to_owned()
    }

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn base(doc: &str, emit: &str, attempt: u32) -> DraftCheckConfig {
        cfg(json!({
            "document_type": doc,
            "emit": emit,
            "attempt": attempt,
            "sources": [{"var": "content_brief"}]
        }))
    }

    const CLEAN: &str = "Dear Returns Department,\n\nThe drill stopped working after two days. \
                         I bought it on 2 November at the Leeds store and ask for a refund.\n\n\
                         Sincerely";
    const INVENTED: &str = "Dear Returns Department,\n\nThe drill stopped working after two \
                            days. I bought it on 2 November at the Leeds store. Mr Harlow in \
                            Seattle agreed.\n\nSincerely";

    #[test]
    fn the_verdict_follows_decide_for_strict_framed_and_free() {
        let v = values(&[("content_brief", &brief())]);
        let run = |doc: &str, attempt: u32, draft: &str| {
            evaluate(&base(doc, "verdict", attempt), Some(draft), &lookup(&v))
                .unwrap()
                .verdict_literal()
        };
        assert_eq!(run("rma_letter", 0, CLEAN), "accept");
        assert_eq!(run("rma_letter", 0, INVENTED), "regenerate");
        assert_eq!(run("rma_letter", 1, INVENTED), "mark");
        assert_eq!(run("cover_letter", 0, INVENTED), "mark");
        assert_eq!(run("creative_story", 0, INVENTED), "skipped");
    }

    #[test]
    fn the_run_settings_replace_the_rows_defaults() {
        let v = values(&[("content_brief", &brief())]);
        let mut c = base("rma_letter", "verdict", 0);
        c.run = Some(RunCheckSettings {
            document_type: "rma_letter".to_owned(),
            latitude: Latitude::Framed,
            pacing: "brisk".to_owned(),
        });
        c.validate(false).unwrap();
        let o = evaluate(&c, Some(INVENTED), &lookup(&v)).unwrap();
        assert_eq!(o.verdict, Verdict::Mark, "framed from the run, not strict");
        // An authored run is refused at LOAD.
        assert!(c
            .validate(true)
            .unwrap_err()
            .contains("may not be authored"));
    }

    #[test]
    fn a_missing_source_marks_and_never_regenerates_or_accepts() {
        let empty = HashMap::new();
        let o = evaluate(
            &base("rma_letter", "verdict", 0),
            Some(INVENTED),
            &lookup(&empty),
        )
        .unwrap();
        assert_eq!(o.verdict, Verdict::Mark);
        assert_eq!(o.missing_sources, ["content_brief"]);
        let o = evaluate(
            &base("rma_letter", "verdict", 0),
            Some("Plain."),
            &lookup(&empty),
        )
        .unwrap();
        assert_eq!(o.verdict, Verdict::Mark);
        assert!(o
            .report_text()
            .contains("source 'content_brief' was not available"));
    }

    #[test]
    fn a_missing_draft_is_skipped() {
        let v = values(&[("content_brief", &brief())]);
        let o = evaluate(&base("rma_letter", "verdict", 0), None, &lookup(&v)).unwrap();
        assert_eq!(o.verdict_literal(), "skipped");
        assert!(o.draft_missing);
    }

    #[test]
    fn the_report_is_content_free() {
        let v = values(&[("content_brief", &brief())]);
        let o = evaluate(
            &base("rma_letter", "report", 0),
            Some(INVENTED),
            &lookup(&v),
        )
        .unwrap();
        let text = o.report_text();
        assert!(text.starts_with("Draft check: regenerate."), "{text}");
        assert!(text.contains("unsupported specific"), "{text}");
        for word in ["Harlow", "Seattle", "drill", "Leeds", "refund"] {
            assert!(!text.contains(word), "{word} leaked into {text}");
        }
        assert!(text.contains("2 of 2 brief points covered"), "{text}");
    }

    #[test]
    fn coverage_is_weighted_per_source_and_never_a_finding() {
        let v = values(&[
            (
                "content_brief",
                "- the drill stopped working\n- a refund is wanted",
            ),
            ("other_points", "- warranty registration number"),
        ]);
        let c = cfg(json!({
            "document_type": "rma_letter", "emit": "report", "attempt": 0,
            "sources": [{"var": "content_brief", "weight": 3.0}, {"var": "other_points", "weight": 1.0}]
        }));
        let draft = "The drill stopped working, so a refund is wanted.";
        let o = evaluate(&c, Some(draft), &lookup(&v)).unwrap();
        assert_eq!((o.coverage.covered, o.coverage.total), (2, 3));
        assert!(
            (o.coverage.weighted - 0.75).abs() < 1e-6,
            "{}",
            o.coverage.weighted
        );
        assert_eq!(
            o.verdict,
            Verdict::Accept,
            "a missing point is not a finding"
        );
    }

    #[test]
    fn essay_citations_mark_only_when_unsupplied() {
        let supplied = "- Kim 2019 found a rise [1]\n[1] Kim, 2019";
        let v = values(&[("content_brief", supplied)]);
        let c = base("essay_section", "verdict", 0);
        let draft = "Kim found a rise in output [1].";
        let o = evaluate(&c, Some(draft), &lookup(&v)).unwrap();
        assert_eq!(o.verdict, Verdict::Accept, "{:?}", o.report);
        let draft = "Kim found a rise in output [1] and a later fall [2a].";
        let o = evaluate(&c, Some(draft), &lookup(&v)).unwrap();
        assert_eq!(o.report.unsupplied_references, ["2a"]);
        assert!(o.report.placeholders.is_empty() && o.report.unsupported.is_empty());
        assert_eq!(o.verdict, Verdict::Mark, "never regenerate for a citation");
        // The same draft as an RMA letter keeps the pre-903 quirk.
        let o = evaluate(&base("rma_letter", "verdict", 0), Some(draft), &lookup(&v)).unwrap();
        assert_eq!(o.report.placeholders, ["[2a]"]);
        assert_eq!(o.verdict, Verdict::Regenerate);
    }

    #[test]
    fn the_section_check_marks_off_topic_and_extra_headings() {
        let v = values(&[
            ("content_brief", "- methods and results"),
            ("next_section", "## Methods\nHow the samples were taken"),
        ]);
        let c = cfg(json!({
            "document_type": "personal_note", "emit": "verdict", "attempt": 0,
            "sources": [{"var": "content_brief", "weight": 0.0}],
            "section_var": "next_section"
        }));
        assert_eq!(c.declared(), ["content_brief", "next_section"]);
        let on = "## Methods\nThe methods were plain.";
        let o = evaluate(&c, Some(on), &lookup(&v)).unwrap();
        assert_eq!(o.section, Some(SectionFindings::default()));
        assert_eq!(o.verdict, Verdict::Accept);
        let ahead = "## Methods\nThe methods were plain.\n## Results\nThey worked.";
        let o = evaluate(&c, Some(ahead), &lookup(&v)).unwrap();
        assert_eq!(o.section.as_ref().unwrap().extra_headings, 1);
        assert_eq!(o.verdict, Verdict::Mark);
        let off = "The weather was fine.";
        let o = evaluate(&c, Some(off), &lookup(&v)).unwrap();
        assert!(o.section.as_ref().unwrap().off_topic);
        assert_eq!(o.verdict, Verdict::Mark);
    }

    #[test]
    fn the_voice_check_marks_stock_phrases_and_long_sentences() {
        let v = values(&[("content_brief", "- thanks for the visit")]);
        let mut c = cfg(json!({
            "document_type": "personal_note", "emit": "verdict", "attempt": 0,
            "sources": [{"var": "content_brief"}], "voice_check": true
        }));
        let o = evaluate(
            &c,
            Some("Thanks for the visit. It was lovely."),
            &lookup(&v),
        )
        .unwrap();
        assert_eq!(o.voice, Some(VoiceFindings::default()));
        let o = evaluate(
            &c,
            Some("I hope this finds you well. Thanks for the visit."),
            &lookup(&v),
        )
        .unwrap();
        assert_eq!(o.voice.as_ref().unwrap().stock_phrases, 1);
        assert_eq!(o.verdict, Verdict::Mark);
        let long = format!("Thanks for the visit {}.", "and the lovely time ".repeat(6));
        c.run = Some(RunCheckSettings {
            document_type: "personal_note".to_owned(),
            latitude: Latitude::Framed,
            pacing: "brisk".to_owned(),
        });
        let o = evaluate(&c, Some(&long), &lookup(&v)).unwrap();
        assert!(o.voice.as_ref().unwrap().sentences_too_long);
        c.run.as_mut().unwrap().pacing = "unhurried".to_owned();
        let o = evaluate(&c, Some(&long), &lookup(&v)).unwrap();
        assert!(!o.voice.as_ref().unwrap().sentences_too_long);
    }

    #[test]
    fn the_config_refuses_what_it_should() {
        for (bad, why) in [
            (
                json!({"document_type": "nope", "emit": "verdict", "attempt": 0, "sources": [{"var": "a"}]}),
                "unknown document_type",
            ),
            (
                json!({"document_type": "rma_letter", "emit": "verdict", "attempt": 2, "sources": [{"var": "a"}]}),
                "attempt",
            ),
            (
                json!({"document_type": "rma_letter", "emit": "verdict", "attempt": 0, "sources": []}),
                "sources",
            ),
            (
                json!({"document_type": "rma_letter", "emit": "verdict", "attempt": 0, "sources": [{"var": "a"}, {"var": "a"}]}),
                "twice",
            ),
            (
                json!({"document_type": "rma_letter", "emit": "verdict", "attempt": 0, "sources": [{"var": "A"}]}),
                "lower-case",
            ),
            (
                json!({"document_type": "rma_letter", "emit": "verdict", "attempt": 0, "sources": [{"var": "a", "weight": -1.0}]}),
                "weight",
            ),
        ] {
            let c: DraftCheckConfig = serde_json::from_value(bad).unwrap();
            assert!(c.validate(true).unwrap_err().contains(why), "{why}");
        }
        assert!(
            serde_json::from_value::<DraftCheckConfig>(json!({
                "document_type": "rma_letter", "emit": "verdict", "attempt": 0,
                "sources": [{"var": "a"}], "latitude": "free"
            }))
            .is_err(),
            "no unknown key"
        );
    }

    #[test]
    fn with_run_settings_fills_run_and_leaves_rows_without_a_section_alone() {
        let row = json!({"input_var": "d", "output_var": "v", "criteria": [], "on_fail": "advisory",
            "draft_check": {"document_type": "rma_letter", "emit": "verdict", "attempt": 0,
            "sources": [{"var": "content_brief"}]}});
        let run = RunCheckSettings {
            document_type: "cover_letter".to_owned(),
            latitude: Latitude::Framed,
            pacing: "measured".to_owned(),
        };
        let out = with_run_settings(&row, run.clone()).unwrap().unwrap();
        assert_eq!(out["draft_check"]["run"]["latitude"], "framed");
        assert_eq!(out["draft_check"]["document_type"], "rma_letter");
        assert!(with_run_settings(&json!({"input_var": "d"}), run)
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_module_is_pure() {
        let s = include_str!("check_block.rs");
        let prod = s.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "sqlx",
            "reqwest",
            "tokio",
            "std::fs",
            "std::net",
            "tauri::",
            "provider_selection",
            "model_resolver",
            "ollama",
        ] {
            assert!(!prod.contains(banned), "check_block.rs names '{banned}'");
        }
    }
}

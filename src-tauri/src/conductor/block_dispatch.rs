// src-tauri/src/conductor/block_dispatch.rs
//
// items.id=129 (Stage 1): the compiled-in block handler registry.
//
// A composition row (shared_021.sql, focus_block_compositions) may cite a
// building block by stable_id. Two independent facts must hold before such a
// row can run, and the loader checks both at LOAD, with distinct errors:
//   1. MEMBERSHIP -- the stable_id is in shared.db's building_blocks mirror
//      as an inline_composable block (focus_composition_store::
//      is_inline_composable_block). The mirror is validation-only data.
//   2. HANDLER -- the stable_id has compiled-in Rust code, i.e.
//      block_handler(stable_id) is Some. A cataloged block with no handler
//      (cb-02, pc-03, cb-10, ...) is valid catalog data that cannot run yet,
//      so a Focus citing it fails at LOAD, never mid-run.
//
// block_handler() is the single registry: it is the existence check the loader
// uses AND the table execute_step() dispatches through, so the two cannot
// drift apart.
//
// Each handler owns a typed customization struct with deny_unknown_fields.
// BlockHandler::validate parses the row's customization JSON into it at LOAD,
// so a misspelled or unknown key fails at LOAD rather than being ignored.
//
// A block step is NOT an LLM step. Its handler is a pure function over the
// run's TaskTrack: it makes no provider call, touches no PersonalTrack and no
// disclosure buffer, and reads only a named output variable. Every block step
// is authored LocalOnly (load_focus_definition_from_db sets routing_tier
// LocalOnly and no external_access_override), so it never reaches F2 /
// OfferTier2 or any external-step path.

use serde::Deserialize;

use crate::conductor::quality::{CriterionKind, QualityAssessor, QualityCriterion};
use crate::conductor::types::TaskTrack;

/// What a handler produced. execute_step() records it with
/// TaskTrack::add_step; a handler never mutates the track itself.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockOutput {
    /// Becomes TaskStep.content.
    pub content: String,
    /// Becomes TaskStep.output_var (the name later steps read it under).
    pub output_var: String,
}

/// One compiled-in block.
pub struct BlockHandler {
    pub stable_id: &'static str,
    /// LOAD-time check of a row's customization JSON against the handler's
    /// typed struct. Err carries a message for the loader to wrap.
    pub validate: fn(&serde_json::Value) -> Result<(), String>,
    /// Run the block over the track. Err only for a customization that did not
    /// validate (LOAD guarantees it did) -- never for a quality verdict.
    pub run: fn(&serde_json::Value, &TaskTrack) -> Result<BlockOutput, String>,
}

/// The registry. Handlers: cb-06 only (items.id=129 Stage 1).
pub fn block_handler(stable_id: &str) -> Option<&'static BlockHandler> {
    match stable_id {
        "cb-06" => Some(&CB06),
        _ => None,
    }
}

/// Every stable_id that has a handler. Used by tests to hold the registry and
/// the building_blocks mirror together.
pub const REGISTERED_STABLE_IDS: &[&str] = &["cb-06"];

// ---------------------------------------------------------------------------
// cb-06 -- Quality/completeness assessment
// ---------------------------------------------------------------------------

static CB06: BlockHandler = BlockHandler {
    stable_id: "cb-06",
    validate: cb06_validate,
    run: cb06_run,
};

/// The only on_fail value cb-06 accepts. The block reports a verdict and never
/// retries, routes or raises; anything stronger (raising QualityBelowFloor
/// into FailureHandler's F2 branch) would be external-step-adjacent behavior
/// and is not built (items.id=626 waiver).
pub const CB06_ON_FAIL_ADVISORY: &str = "advisory";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cb06Config {
    /// Name of the earlier step's output_var to assess.
    input_var: String,
    /// Name the verdict text is recorded under. Must differ from input_var.
    output_var: String,
    criteria: Vec<Cb06Criterion>,
    on_fail: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cb06Criterion {
    /// Surfaced in the verdict text. Authored config, never draft content.
    name: String,
    kind: Cb06Kind,
    /// Required for min_length, forbidden for non_empty.
    min_length: Option<usize>,
    /// Defaults to 1.0. 0.0 keeps the criterion visible but never failing.
    weight: Option<f32>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Cb06Kind {
    NonEmpty,
    MinLength,
}

fn parse_cb06(customization: &serde_json::Value) -> Result<Cb06Config, String> {
    let cfg: Cb06Config =
        serde_json::from_value(customization.clone()).map_err(|e| format!("cb-06 config: {e}"))?;

    if cfg.input_var.trim().is_empty() {
        return Err("cb-06 config: input_var is empty".to_owned());
    }
    if cfg.output_var.trim().is_empty() {
        return Err("cb-06 config: output_var is empty".to_owned());
    }
    // collect_output_vars() is last-write-wins by name, so a verdict recorded
    // under the input's own name would replace the draft it assessed.
    if cfg.input_var == cfg.output_var {
        return Err(format!(
            "cb-06 config: output_var must differ from input_var ('{}'), or the verdict would \
             replace the assessed content",
            cfg.input_var
        ));
    }
    if crate::conductor::tokens::is_system_token(&cfg.output_var) {
        return Err(format!(
            "cb-06 config: output_var '{}' collides with a system token",
            cfg.output_var
        ));
    }
    if cfg.on_fail != CB06_ON_FAIL_ADVISORY {
        return Err(format!(
            "cb-06 config: on_fail must be '{CB06_ON_FAIL_ADVISORY}' (the only supported value), \
             got '{}'",
            cfg.on_fail
        ));
    }
    if cfg.criteria.is_empty() {
        return Err("cb-06 config: criteria is empty".to_owned());
    }
    for c in &cfg.criteria {
        if c.name.trim().is_empty() {
            return Err("cb-06 config: a criterion has an empty name".to_owned());
        }
        match (c.kind, c.min_length) {
            (Cb06Kind::MinLength, None) => {
                return Err(format!(
                    "cb-06 config: criterion '{}' is min_length and needs min_length",
                    c.name
                ))
            }
            (Cb06Kind::NonEmpty, Some(_)) => {
                return Err(format!(
                    "cb-06 config: criterion '{}' is non_empty and must not set min_length",
                    c.name
                ))
            }
            _ => {}
        }
        if let Some(w) = c.weight {
            if !w.is_finite() || w < 0.0 {
                return Err(format!(
                    "cb-06 config: criterion '{}' weight must be a finite number >= 0",
                    c.name
                ));
            }
        }
    }
    Ok(cfg)
}

fn cb06_validate(customization: &serde_json::Value) -> Result<(), String> {
    parse_cb06(customization).map(|_| ())
}

/// Pure over (customization, task_track.get_output(input_var)). Advisory only:
/// a failing or missing input yields a verdict, never an error.
///
/// The verdict text carries criterion names (authored config), counts and the
/// input variable's NAME. It never carries the assessed content, and it does
/// not use QualityGap::detail, whose wording is the assessor's own and is not
/// guaranteed to stay content-free.
fn cb06_run(
    customization: &serde_json::Value,
    task_track: &TaskTrack,
) -> Result<BlockOutput, String> {
    let cfg = parse_cb06(customization)?;

    let criteria: Vec<QualityCriterion> = cfg
        .criteria
        .iter()
        .map(|c| {
            let kind = match c.kind {
                Cb06Kind::NonEmpty => CriterionKind::NonEmpty,
                // parse_cb06 guarantees Some for min_length.
                Cb06Kind::MinLength => CriterionKind::MinLength(c.min_length.unwrap_or(0)),
            };
            QualityCriterion::new(c.name.clone(), kind).with_weight(c.weight.unwrap_or(1.0))
        })
        .collect();
    let total = criteria.len();

    let input = task_track.get_output(&cfg.input_var);
    let verdict = QualityAssessor::new(criteria).assess_content(input.unwrap_or(""));

    let unmet = verdict.gaps.len();
    let names = verdict
        .gaps
        .iter()
        .map(|g| g.criterion_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let mut text = format!(
        "Quality check (advisory): {}. {} of {} criteria met, score {:.2}.",
        if verdict.passed {
            "passed"
        } else {
            "not passed"
        },
        total - unmet,
        total,
        verdict.score
    );
    if input.is_none() {
        text.push_str(&format!(" Input '{}' was not available.", cfg.input_var));
    }
    if unmet > 0 {
        text.push_str(&format!(" Unmet: {names}."));
    }

    Ok(BlockOutput {
        content: text,
        output_var: cfg.output_var,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::types::TaskStep;
    use serde_json::json;

    fn track_with(var: &str, content: &str) -> TaskTrack {
        let mut t = TaskTrack::new();
        t.add_step(TaskStep {
            action_id: "draft_writing".to_owned(),
            output_var: Some(var.to_owned()),
            content: content.to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 2,
            personal_flagged: false,
            audit_unavailable: false,
        });
        t
    }

    fn cfg(criteria: serde_json::Value) -> serde_json::Value {
        json!({
            "input_var": "draft_output",
            "output_var": "quality_verdict",
            "criteria": criteria,
            "on_fail": "advisory"
        })
    }

    fn run(c: &serde_json::Value, t: &TaskTrack) -> BlockOutput {
        (block_handler("cb-06").unwrap().run)(c, t).expect("cb-06 must not raise")
    }

    #[test]
    fn registry_knows_cb06_only() {
        assert!(block_handler("cb-06").is_some());
        for id in ["cb-01", "cb-02", "cb-10", "pc-03", "pc-04", "nope", ""] {
            assert!(block_handler(id).is_none(), "{id} must have no handler");
        }
        for id in REGISTERED_STABLE_IDS {
            assert_eq!(block_handler(id).unwrap().stable_id, *id);
        }
    }

    #[test]
    fn pass_case() {
        let t = track_with(
            "draft_output",
            "A perfectly reasonable draft of some length.",
        );
        let c = cfg(json!([
            {"name": "has_content", "kind": "non_empty"},
            {"name": "long_enough", "kind": "min_length", "min_length": 10}
        ]));
        let out = run(&c, &t);
        assert_eq!(out.output_var, "quality_verdict");
        assert!(out.content.contains("passed."), "{}", out.content);
        assert!(out.content.contains("2 of 2 criteria met"));
        assert!(!out.content.contains("Unmet"));
    }

    #[test]
    fn advisory_fail_reports_and_does_not_raise() {
        let t = track_with("draft_output", "short");
        let c = cfg(json!([
            {"name": "has_content", "kind": "non_empty"},
            {"name": "long_enough", "kind": "min_length", "min_length": 500}
        ]));
        let out = run(&c, &t);
        assert!(out.content.contains("not passed"), "{}", out.content);
        assert!(out.content.contains("1 of 2 criteria met"));
        assert!(out.content.contains("Unmet: long_enough."));
    }

    #[test]
    fn weight_zero_is_listed_but_does_not_fail() {
        let t = track_with("draft_output", "short");
        let c = cfg(json!([
            {"name": "has_content", "kind": "non_empty"},
            {"name": "nice_to_be_long", "kind": "min_length", "min_length": 500, "weight": 0.0}
        ]));
        let out = run(&c, &t);
        assert!(out.content.contains("passed."), "{}", out.content);
        assert!(!out.content.contains("not passed"));
        assert!(out.content.contains("Unmet: nice_to_be_long."));
    }

    #[test]
    fn missing_input_var_yields_a_verdict_not_an_error() {
        let t = track_with("something_else", "text");
        let c = cfg(json!([{"name": "has_content", "kind": "non_empty"}]));
        let out = run(&c, &t);
        assert!(out.content.contains("not passed"), "{}", out.content);
        assert!(out
            .content
            .contains("Input 'draft_output' was not available."));
        // An empty track too.
        let out = run(&c, &TaskTrack::new());
        assert!(out.content.contains("not passed"));
    }

    #[test]
    fn verdict_never_contains_draft_text() {
        // A marker no template or criterion name could produce.
        let marker = "ZX9-PRIVATE-DRAFT-MARKER";
        let draft = format!("{marker} {marker} {marker}");
        let t = track_with("draft_output", &draft);
        for criteria in [
            json!([{"name": "has_content", "kind": "non_empty"}]),
            json!([{"name": "long_enough", "kind": "min_length", "min_length": 9999}]),
            json!([
                {"name": "a", "kind": "non_empty"},
                {"name": "b", "kind": "min_length", "min_length": 9999, "weight": 0.0}
            ]),
        ] {
            let out = run(&cfg(criteria), &t);
            assert!(
                !out.content.contains(marker),
                "verdict leaked draft text: {}",
                out.content
            );
        }
    }

    #[test]
    fn run_does_not_mutate_the_track() {
        let t = track_with("draft_output", "text");
        let before = t.steps().len();
        let _ = run(&cfg(json!([{"name": "n", "kind": "non_empty"}])), &t);
        assert_eq!(t.steps().len(), before);
    }

    #[test]
    fn validate_rejects_bad_config() {
        let v = block_handler("cb-06").unwrap().validate;
        let good = cfg(json!([{"name": "n", "kind": "non_empty"}]));
        assert!(v(&good).is_ok());

        // Unknown top-level key.
        let mut bad = good.clone();
        bad["extra"] = json!(1);
        assert!(v(&bad).unwrap_err().contains("unknown field"));

        // Unknown criterion key.
        assert!(v(&cfg(json!([{"name": "n", "kind": "non_empty", "mn": 1}]))).is_err());

        // Missing required key.
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove("input_var");
        assert!(v(&bad).unwrap_err().contains("input_var"));

        // on_fail anything but advisory, including absent.
        for on_fail in ["retry", "fail", "Advisory", ""] {
            let mut bad = good.clone();
            bad["on_fail"] = json!(on_fail);
            assert!(v(&bad).unwrap_err().contains("on_fail"), "{on_fail}");
        }
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove("on_fail");
        assert!(v(&bad).is_err());

        // output_var naming a system token.
        let mut bad = good.clone();
        bad["output_var"] = json!("previous_output");
        assert!(v(&bad).unwrap_err().contains("system token"));

        // output_var == input_var.
        let mut bad = good.clone();
        bad["output_var"] = json!("draft_output");
        assert!(v(&bad).unwrap_err().contains("must differ"));

        // Empty criteria; unknown kind; min_length mismatches; bad weights.
        assert!(v(&cfg(json!([]))).is_err());
        assert!(v(&cfg(json!([{"name": "n", "kind": "required_fields"}]))).is_err());
        assert!(v(&cfg(json!([{"name": "n", "kind": "min_length"}]))).is_err());
        assert!(v(&cfg(
            json!([{"name": "n", "kind": "non_empty", "min_length": 3}])
        ))
        .is_err());
        assert!(v(&cfg(
            json!([{"name": "n", "kind": "non_empty", "weight": -1.0}])
        ))
        .is_err());
        assert!(v(&cfg(json!([{"name": " ", "kind": "non_empty"}]))).is_err());

        // Not an object at all.
        assert!(v(&json!([1, 2])).is_err());
        assert!(v(&json!({})).is_err());
    }
}

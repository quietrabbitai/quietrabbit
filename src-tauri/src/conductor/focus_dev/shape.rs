// src-tauri/src/conductor/focus_dev/shape.rs
//
// Shape checks over an expanded FocusSeed (items.id=898, decisions.id=951). They catch what
// would otherwise reach the database as a raw CHECK failure or reach the loader as a silently
// ignored key: closed vocabularies the tables enforce, uniqueness, guard and store-column
// well-formedness (through the REAL parsers, conductor::guard), and unknown keys in an
// ordinary step's customization. Everything the loader itself validates (block registration,
// declared-inputs lint, placement, guard paths, launch lint) is NOT repeated here: stage 3's
// validate command runs the real loader.

use std::collections::BTreeSet;

use crate::conductor::focus_dev::FocusDevError;
use crate::conductor::guard;
use crate::conductor::lifecycle::ORDINARY_STEP_KEYS;
use crate::conductor::model_block::config::is_slug;
use crate::persistence::focus_composition_store::FocusSeed;

const TIERS: &[&str] = &[
    "local_only",
    "anonymous_required",
    "anonymous_preferred",
    "unrestricted",
];
const STATUSES: &[&str] = &["designed", "in_build", "shipped", "deferred"];
const POSTURES: &[&str] = &["per_field", "focus_level"];
const SLOTS: &[&str] = &[
    "document_type",
    "launch_mode",
    "launch_answer_1",
    "launch_answer_2",
    "launch_answer_3",
];
const MAX_QUESTIONS: usize = 5;

/// A Focus id: lowercase, digits and hyphens (writing-assistant, tech-support).
pub fn is_focus_id(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && s.len() <= 64
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-')
}

/// Every problem found, or Ok.
pub fn check(seed: &FocusSeed) -> Result<(), FocusDevError> {
    let problems = problems(seed);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(FocusDevError::Shape(problems))
    }
}

pub fn problems(seed: &FocusSeed) -> Vec<String> {
    let mut p = Vec::new();
    let f = &seed.focus;
    if !is_focus_id(&f.focus_id) {
        p.push(format!("focus_id '{}' is not a lowercase id", f.focus_id));
    }
    if f.display_name.trim().is_empty() {
        p.push("display_name is empty".to_owned());
    }
    if f.version.trim().is_empty() {
        p.push("version is empty".to_owned());
    }
    if !TIERS.contains(&f.max_routing_tier.as_str()) {
        p.push(format!(
            "max_routing_tier '{}' is not one of {TIERS:?}",
            f.max_routing_tier
        ));
    }
    if !STATUSES.contains(&f.status.as_str()) {
        p.push(format!("status '{}' is not one of {STATUSES:?}", f.status));
    }
    if !POSTURES.contains(&f.collection_posture.as_str()) {
        p.push(format!(
            "collection_posture '{}' is not one of {POSTURES:?}",
            f.collection_posture
        ));
    }
    if f.high_priority_trigger_anchor_field.is_some() != f.high_priority_trigger_offset.is_some() {
        p.push("the high-priority trigger needs both anchor_field and offset".to_owned());
    }
    if let Some(t) = &seed.ui_one_liner {
        let n = t.chars().count();
        if !(1..=80).contains(&n) {
            p.push(format!("ui_one_liner is {n} characters, must be 1 to 80"));
        }
    }
    if seed.compositions.is_empty() {
        p.push("the Focus has no rows".to_owned());
    }

    let mut seen = BTreeSet::new();
    for c in &seed.compositions {
        let id = c.action_id.as_str();
        if !is_slug(id) {
            p.push(format!(
                "row '{id}': action_id is not a lowercase identifier"
            ));
        }
        if !seen.insert(id) {
            p.push(format!("row '{id}': duplicate action_id"));
        }
        match serde_json::from_str::<serde_json::Value>(&c.customization) {
            Ok(serde_json::Value::Object(m)) => {
                if c.block_stable_id.is_none() {
                    for k in m.keys() {
                        if !ORDINARY_STEP_KEYS.contains(&k.as_str()) {
                            p.push(format!("row '{id}': unknown ordinary-step key '{k}'"));
                        }
                    }
                }
            }
            _ => p.push(format!("row '{id}': customization is not a JSON object")),
        }
        match guard::Guard::from_columns(
            c.guard_var.as_deref(),
            c.guard_op.as_deref(),
            c.guard_value.as_deref(),
        ) {
            Ok(single) => match guard::parse_clauses(c.guard_clauses.as_deref()) {
                Ok(clauses) => {
                    if let Err(e) = guard::check_clause_total(single.as_ref(), &clauses) {
                        p.push(format!("row '{id}': {e}"));
                    }
                }
                Err(e) => p.push(format!("row '{id}': guard_clauses: {e}")),
            },
            Err(e) => p.push(format!("row '{id}': guard: {e}")),
        }
        if c.store_op.is_none() && (c.store_target.is_some() || c.store_args.is_some()) {
            p.push(format!(
                "row '{id}': store target or args set without a store op"
            ));
        }
        if c.schedule_trigger_anchor_field.is_some() != c.schedule_trigger_offset.is_some() {
            p.push(format!(
                "row '{id}': a schedule trigger needs both anchor_field and offset"
            ));
        }
    }

    if seed.questions.len() > MAX_QUESTIONS {
        p.push(format!(
            "{} questions, at most {MAX_QUESTIONS}",
            seed.questions.len()
        ));
    }
    let mut qids = BTreeSet::new();
    let mut slots = BTreeSet::new();
    for q in &seed.questions {
        let id = q.question_id.as_str();
        if !is_slug(id) || !qids.insert(id) {
            p.push(format!(
                "question '{id}': id must be a unique lowercase identifier"
            ));
        }
        if !SLOTS.contains(&q.slot.as_str()) {
            p.push(format!(
                "question '{id}': slot '{}' is not one of {SLOTS:?}",
                q.slot
            ));
        } else if !slots.insert(q.slot.as_str()) {
            p.push(format!(
                "question '{id}': slot '{}' is used by another question",
                q.slot
            ));
        }
        if !matches!(q.kind.as_str(), "mode" | "choice") {
            p.push(format!(
                "question '{id}': kind '{}' is not mode or choice",
                q.kind
            ));
        }
        if q.kind == "mode" && q.slot != "launch_mode" {
            p.push(format!(
                "question '{id}': a mode question uses the launch_mode slot"
            ));
        }
        if q.options.is_empty() {
            p.push(format!("question '{id}': no options"));
        }
        let mut oids = BTreeSet::new();
        for o in &q.options {
            if !oids.insert(o.option_id.as_str()) {
                p.push(format!(
                    "question '{id}': duplicate option '{}'",
                    o.option_id
                ));
            }
            if o.envelope.is_some() && q.kind != "mode" {
                p.push(format!(
                    "question '{id}': option '{}' has an envelope but the question is not a mode",
                    o.option_id
                ));
            }
        }
        if !q.options.iter().any(|o| o.option_id == q.default_option) {
            p.push(format!(
                "question '{id}': default '{}' is not one of its options",
                q.default_option
            ));
        }
    }
    p
}

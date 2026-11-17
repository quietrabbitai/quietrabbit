// src-tauri/examples/harness/voice.rs
//
// DEV-ONLY (items.id=899 stage 3). The quick_response half of the harness: the
// voice block's task type (cb-13), measured through the SHIPPED block, in both
// modes. Included by calibration_harness.rs as `mod voice`.
//
//   apply   a finished synthetic draft is rewritten in a voice. The facts of the
//           draft are the coverage points; the allowed text is the draft plus the
//           voice profile, so a rewrite that adds a name, number or place is an
//           unsupported specific.
//   refine  no draft: the step writes a few sentences of guidance from the voice
//           profile and the document type's settings. Allowed text is the profile
//           and the settings the prompt itself carries.
//
// The cell this produces is task type quick_response, profile "" (task-type-wide),
// one per model, because the voice step's document profile varies and the resolver
// falls back to the task-type-wide cell. The two modes are read separately and the
// cell takes the lower of the two (bars.json, rollup.quick_response).

use std::collections::BTreeMap;

use quietrabbit_lib::conductor::model_block::document_type::DocumentType;
use quietrabbit_lib::conductor::model_block::{model_block_spec, ExpandInput};
use quietrabbit_lib::conductor::tokens::{ActionDefinition, ExternalAccess};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::support::render_template;
use crate::{document_type, PointData};

const VOICE_JSON: &str = include_str!("../calibration_data/voice.json");

pub const GROUP_APPLY: &str = "voice_apply";
pub const GROUP_REFINE: &str = "voice_refine";
pub const ACTION_ID: &str = "calibration_voice";

#[derive(Deserialize)]
pub struct VoiceCorpus {
    pub profiles: Vec<Profile>,
    pub apply: Vec<ApplyCase>,
    pub refine: Vec<RefineCase>,
}

#[derive(Deserialize)]
pub struct Profile {
    pub id: String,
    pub text: String,
}

#[derive(Deserialize)]
pub struct ApplyCase {
    pub id: String,
    pub document_type: String,
    pub profile: String,
    pub draft: String,
    pub points: Vec<PointData>,
}

#[derive(Deserialize)]
pub struct RefineCase {
    pub id: String,
    pub document_type: String,
    pub profile: String,
}

pub fn corpus() -> VoiceCorpus {
    serde_json::from_str(VOICE_JSON).expect("calibration_data/voice.json parses")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Apply,
    Refine,
}

impl Mode {
    pub fn group(self) -> &'static str {
        match self {
            Mode::Apply => GROUP_APPLY,
            Mode::Refine => GROUP_REFINE,
        }
    }
}

fn voice_row(t: &DocumentType, mode: Mode) -> Value {
    let (declared, mode_s, input, out): (Vec<&str>, &str, Value, &str) = match mode {
        Mode::Apply => (
            vec!["voice_profile", "draft_output"],
            "apply",
            json!("draft_output"),
            "final_output",
        ),
        Mode::Refine => (
            vec!["voice_profile"],
            "refine",
            Value::Null,
            "refined_guidance",
        ),
    };
    json!({
        "common": {
            "display_name": "Calibration voice",
            "output_var": out,
            "need": {"task_type": "quick_response", "difficulty": "low", "output_size": "short"},
            "declared_inputs": declared,
            "range": {
                "quality_floor": {"min": "basic", "max": "standard"},
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {"document_type": t.id, "mode": mode_s, "input_var": input}
    })
}

/// The ordinary step the shipped voice block expands into, with the type's
/// shipped default settings.
pub fn expand_voice(t: &DocumentType, mode: Mode) -> ActionDefinition {
    let spec = model_block_spec("cb-13").expect("cb-13 is registered");
    let parsed = (spec.parse)(&voice_row(t, mode), false)
        .unwrap_or_else(|e| panic!("cb-13 row for {} does not parse: {e}", t.id));
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
    .unwrap_or_else(|e| panic!("cb-13 for {} does not expand: {e}", t.id))
}

pub struct VoiceRendered {
    pub prompt: String,
    /// What the output may draw its specifics from.
    pub allowed_text: String,
}

fn profile_text<'a>(c: &'a VoiceCorpus, id: &str) -> &'a str {
    &c.profiles
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("voice profile {id} is not in voice.json"))
        .text
}

pub fn render_apply(step: &ActionDefinition, c: &VoiceCorpus, case: &ApplyCase) -> VoiceRendered {
    let profile = profile_text(c, &case.profile);
    let tokens = BTreeMap::from([
        ("voice_profile", profile.to_owned()),
        ("draft_output", case.draft.clone()),
    ]);
    VoiceRendered {
        prompt: render_template(&step.prompt_template, &tokens),
        allowed_text: format!("{}\n{}", case.draft, profile),
    }
}

pub fn render_refine(step: &ActionDefinition, c: &VoiceCorpus, case: &RefineCase) -> VoiceRendered {
    let profile = profile_text(c, &case.profile);
    let tokens = BTreeMap::from([("voice_profile", profile.to_owned())]);
    let prompt = render_template(&step.prompt_template, &tokens);
    VoiceRendered {
        // The prompt carries the type's settings phrases, which the guidance may repeat.
        allowed_text: prompt.clone(),
        prompt,
    }
}

/// Every (case id, mode, step, rendered) of the voice corpus, in corpus order.
pub fn all_cases(c: &VoiceCorpus) -> Vec<(String, Mode, String, ActionDefinition, VoiceRendered)> {
    let mut out = Vec::new();
    for a in &c.apply {
        let t = document_type(&a.document_type);
        let step = expand_voice(t, Mode::Apply);
        let r = render_apply(&step, c, a);
        out.push((a.id.clone(), Mode::Apply, a.document_type.clone(), step, r));
    }
    for a in &c.refine {
        let t = document_type(&a.document_type);
        let step = expand_voice(t, Mode::Refine);
        let r = render_refine(&step, c, a);
        out.push((a.id.clone(), Mode::Refine, a.document_type.clone(), step, r));
    }
    out
}

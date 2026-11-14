// src-tauri/src/conductor/model_block/voice.rs
//
// cb-13, the VOICE model block (decisions.id=941 point 2), in two modes.
//
//   apply   Rewrite a draft so it reads in the writer's voice: a VoiceTransform
//           step that reads the draft variable and {voice_profile}.
//   refine  Turn {voice_profile} and the document type's four voice settings into
//           a few plain sentences of voice guidance for a later draft: a Generate
//           step that reads no draft. (Handoff 632 and decisions.id=942 name
//           "voice (apply, refine)" without defining refine: this is the
//           definition items.id=894 builds on, accepted by Jason.)
//
// The four voice settings (directness, formality, pacing, tone) are named steps
// of document_types.json scales: each step carries the plain phrase put in the
// prompt. Every sentence is data; this file is the skeleton.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::model_block::document_type::registry;
use crate::conductor::model_block::{
    check_var, finish, parse_row, resolve_type, BlockSpec, Built, ExpandInput, ParsedModelBlock,
};
use crate::conductor::tokens::{ActionDefinition, StepType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    Apply,
    Refine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSpec {
    /// A document type id in the registry: its voice settings and drafting
    /// parameters are used.
    pub document_type: String,
    pub mode: VoiceMode,
    /// The declared input holding the draft. Required for apply, null for refine.
    pub input_var: Option<String>,
}

pub(crate) fn parse(value: &Value, external_allowed: bool) -> Result<ParsedModelBlock, String> {
    let (common, spec): (_, VoiceSpec) = parse_row("cb-13", value, external_allowed)?;
    if registry()?.document_type(&spec.document_type).is_none() {
        return Err(format!(
            "cb-13: unknown document_type '{}'",
            spec.document_type
        ));
    }
    match (spec.mode, &spec.input_var) {
        (VoiceMode::Apply, None) => {
            return Err("cb-13: mode apply needs an input_var (the draft to rewrite)".to_owned())
        }
        (VoiceMode::Refine, Some(_)) => {
            return Err("cb-13: mode refine reads no draft, so input_var must be null".to_owned())
        }
        (_, Some(v)) => check_var("cb-13", "input_var", v)?,
        (VoiceMode::Refine, None) => {}
    }
    Ok(ParsedModelBlock {
        common,
        spec: BlockSpec::Voice(spec),
    })
}

pub(crate) fn expand(
    parsed: &ParsedModelBlock,
    input: &ExpandInput,
) -> Result<ActionDefinition, String> {
    let BlockSpec::Voice(spec) = &parsed.spec else {
        return Err("cb-13: expand called on another block's row".to_owned());
    };
    let reg = registry()?;
    let (t, s) = resolve_type(&spec.document_type, input, reg)?;
    let p = &reg.phrases.voice;
    let length = *t.length_words.get(&s.length).ok_or_else(|| {
        format!(
            "document type '{}': no length entry for '{}'",
            t.id, s.length
        )
    })?;

    // resolve_type checked every id exists, so these lookups cannot miss.
    let phrase = |step: Option<&crate::conductor::model_block::document_type::ScaleStep>| {
        step.map(|x| x.phrase.clone()).unwrap_or_default()
    };
    let settings_lines = [
        format!(
            "- {}: {}",
            p.directness_label,
            phrase(reg.scales.directness(&s.directness))
        ),
        format!(
            "- {}: {}",
            p.formality_label,
            phrase(reg.scales.formality(&s.formality))
        ),
        format!(
            "- {}: {}",
            p.pacing_label,
            phrase(reg.scales.pacing(&s.pacing))
        ),
        format!("- {}: {}", p.tone_label, phrase(reg.scales.tone(&s.tone))),
    ];

    let (step_type, lead, output_only) = match spec.mode {
        VoiceMode::Apply => (
            StepType::VoiceTransform,
            &p.apply_lead,
            &p.apply_output_only,
        ),
        VoiceMode::Refine => (StepType::Generate, &p.refine_lead, &p.refine_output_only),
    };
    let mut lines: Vec<String> = vec![
        lead.clone(),
        output_only.clone(),
        String::new(),
        p.settings_label.clone(),
    ];
    lines.extend(settings_lines);
    lines.extend([
        String::new(),
        format!("{} {{voice_profile}}", p.voice_profile_label),
    ]);
    if let (VoiceMode::Apply, Some(v)) = (spec.mode, &spec.input_var) {
        lines.extend([String::new(), p.draft_label.clone(), format!("{{{v}}}")]);
    }

    finish(
        "cb-13",
        parsed,
        input,
        Built {
            step_type,
            template: lines.join("\n"),
            params: t.params.voice,
            length,
        },
    )
}

pub(crate) fn example() -> Value {
    json!({
        "common": {
            "display_name": "Refining your voice...",
            "output_var": "final_output",
            "need": { "task_type": "quick_response", "difficulty": "low", "output_size": "short" },
            "declared_inputs": ["voice_profile", "draft_output"],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "rma_letter",
            "mode": "apply",
            "input_var": "draft_output"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tokens::ExternalAccess;

    fn refine() -> Value {
        let mut v = example();
        v["common"]["output_var"] = json!("voice_guidance");
        v["common"]["declared_inputs"] = json!(["voice_profile"]);
        v["spec"]["mode"] = json!("refine");
        v["spec"]["input_var"] = Value::Null;
        v
    }

    fn expand_json(v: &Value) -> Result<ActionDefinition, String> {
        let parsed = parse(v, false)?;
        expand(
            &parsed,
            &ExpandInput {
                action_id: "voice_step",
                focus_ceiling: ExternalAccess::LocalOnly,
                settings: None,
                external_allowed: false,
            },
        )
    }

    #[test]
    fn apply_is_a_voice_transform_that_reads_the_draft_and_the_profile() {
        let step = expand_json(&example()).unwrap();
        assert_eq!(step.step_type, StepType::VoiceTransform);
        assert_eq!(step.block_stable_id, None);
        assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        let t = &step.prompt_template;
        assert!(t.contains("Keep every fact and all of the meaning"), "{t}");
        assert!(t.contains("{voice_profile}") && t.contains("{draft_output}"));
        // The type's voice settings become their plain phrases.
        assert!(t.contains("- Formality: Write formally"), "{t}");
        assert!(
            t.contains("- Directness: State the main point plainly and early."),
            "{t}"
        );
        assert_eq!(step.options_override["temperature"], json!(0.3));
        assert_eq!(step.requirement.as_ref().unwrap().block_stable_id, "cb-13");
    }

    #[test]
    fn refine_is_a_generate_step_that_reads_only_the_profile() {
        let step = expand_json(&refine()).unwrap();
        assert_eq!(step.step_type, StepType::Generate);
        assert!(step
            .prompt_template
            .contains("Do not write the draft itself."));
        assert!(step.prompt_template.contains("{voice_profile}"));
        assert!(!step.prompt_template.contains("Draft:"));
        assert_eq!(step.declared_inputs, vec!["voice_profile"]);
    }

    #[test]
    fn mode_and_input_var_must_agree() {
        let mut v = example();
        v["spec"]["input_var"] = Value::Null;
        assert!(parse(&v, false).unwrap_err().contains("needs an input_var"));
        let mut v = refine();
        v["spec"]["input_var"] = json!("draft_output");
        assert!(parse(&v, false).unwrap_err().contains("must be null"));
        let mut v = example();
        v["spec"]["mode"] = json!("rewrite");
        assert!(parse(&v, false).unwrap_err().contains("rewrite"));
    }

    #[test]
    fn the_voice_settings_in_force_change_the_phrases() {
        let reg = registry().unwrap();
        let parsed = parse(&example(), false).unwrap();
        let mut s = reg.document_type("rma_letter").unwrap().settings.clone();
        s.formality = "casual".to_owned();
        s.tone = "warm".to_owned();
        let step = expand(
            &parsed,
            &ExpandInput {
                action_id: "voice_step",
                focus_ceiling: ExternalAccess::LocalOnly,
                settings: Some(&s),
                external_allowed: false,
            },
        )
        .unwrap();
        assert!(step.prompt_template.contains("Write the way people talk"));
        assert!(step.prompt_template.contains("Sound friendly and warm."));
        assert!(!step.prompt_template.contains("Write formally"));
    }
}

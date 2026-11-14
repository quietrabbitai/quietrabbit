// src-tauri/src/conductor/model_block/revise.rs
//
// cb-14, the REVISE DOCUMENT model block (decisions.id=941 point 2). It expands
// into an ordinary Generate step that applies only the changes named in one
// declared variable to the draft in another. The document type's latitude applies
// (strict: nothing new may be added), and its revise parameters set the
// temperature. Every sentence is data (document_types.json, phrases.revise).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::model_block::document_type::registry;
use crate::conductor::model_block::{
    check_var, finish, parse_row, resolve_type, BlockSpec, Built, ExpandInput, ParsedModelBlock,
};
use crate::conductor::tokens::{ActionDefinition, StepType};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviseSpec {
    /// A document type id in the registry: its latitude and revise parameters.
    pub document_type: String,
    /// The declared input holding the draft to revise.
    pub draft_var: String,
    /// The declared input holding the requested changes (a user instruction or
    /// confirmed delta items).
    pub change_var: String,
}

pub(crate) fn parse(value: &Value, external_allowed: bool) -> Result<ParsedModelBlock, String> {
    let (common, spec): (_, ReviseSpec) = parse_row("cb-14", value, external_allowed)?;
    check_var("cb-14", "draft_var", &spec.draft_var)?;
    check_var("cb-14", "change_var", &spec.change_var)?;
    if spec.draft_var == spec.change_var {
        return Err("cb-14: draft_var and change_var must differ".to_owned());
    }
    if registry()?.document_type(&spec.document_type).is_none() {
        return Err(format!(
            "cb-14: unknown document_type '{}'",
            spec.document_type
        ));
    }
    Ok(ParsedModelBlock {
        common,
        spec: BlockSpec::Revise(spec),
    })
}

pub(crate) fn expand(
    parsed: &ParsedModelBlock,
    input: &ExpandInput,
) -> Result<ActionDefinition, String> {
    let BlockSpec::Revise(spec) = &parsed.spec else {
        return Err("cb-14: expand called on another block's row".to_owned());
    };
    let reg = registry()?;
    let (t, s) = resolve_type(&spec.document_type, input, reg)?;
    let p = &reg.phrases.revise;
    let length = *t.length_words.get(&s.length).ok_or_else(|| {
        format!(
            "document type '{}': no length entry for '{}'",
            t.id, s.length
        )
    })?;

    let lines = [
        p.lead.clone(),
        p.latitude(s.latitude).to_owned(),
        p.output_only.clone(),
        String::new(),
        p.changes_label.clone(),
        format!("{{{}}}", spec.change_var),
        String::new(),
        p.draft_label.clone(),
        format!("{{{}}}", spec.draft_var),
    ];

    finish(
        "cb-14",
        parsed,
        input,
        Built {
            step_type: StepType::Generate,
            template: lines.join("\n"),
            params: t.params.revise,
            length,
        },
    )
}

pub(crate) fn example() -> Value {
    json!({
        "common": {
            "display_name": "Revising your document...",
            "output_var": "revised_output",
            "need": { "task_type": "creative_writing", "difficulty": "low", "output_size": "short" },
            "declared_inputs": ["change_request", "draft_output"],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "rma_letter",
            "draft_var": "draft_output",
            "change_var": "change_request"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::document_type::Latitude;
    use crate::conductor::tokens::ExternalAccess;

    fn expand_with(v: &Value, latitude: Option<Latitude>) -> Result<ActionDefinition, String> {
        let parsed = parse(v, false)?;
        let mut s = registry()
            .unwrap()
            .document_type("rma_letter")
            .unwrap()
            .settings
            .clone();
        if let Some(l) = latitude {
            s.latitude = l;
        }
        expand(
            &parsed,
            &ExpandInput {
                action_id: "revise_step",
                focus_ceiling: ExternalAccess::LocalOnly,
                settings: Some(&s),
                external_allowed: false,
            },
        )
    }

    #[test]
    fn revise_reads_the_changes_and_the_draft() {
        let step = expand_with(&example(), None).unwrap();
        assert_eq!(step.step_type, StepType::Generate);
        assert_eq!(step.block_stable_id, None);
        let t = &step.prompt_template;
        assert!(t.contains("making only the changes requested"), "{t}");
        assert!(t.contains("{change_request}") && t.contains("{draft_output}"));
        assert!(t.contains("Do not add any claim, number, name, place or date"));
        assert_eq!(step.options_override["temperature"], json!(0.2));
        assert_eq!(step.requirement.as_ref().unwrap().block_stable_id, "cb-14");
    }

    #[test]
    fn latitude_changes_the_revise_sentence_in_either_direction() {
        let strict = expand_with(&example(), Some(Latitude::Strict)).unwrap();
        let framed = expand_with(&example(), Some(Latitude::Framed)).unwrap();
        let free = expand_with(&example(), Some(Latitude::Free)).unwrap();
        assert!(strict.prompt_template.contains("Do not add any claim"));
        assert!(framed.prompt_template.contains("reword for persuasion"));
        assert!(free.prompt_template.contains("add invented detail"));
    }

    #[test]
    fn variables_must_differ_and_be_names() {
        let mut v = example();
        v["spec"]["change_var"] = json!("draft_output");
        assert!(parse(&v, false).unwrap_err().contains("must differ"));
        let mut v = example();
        v["spec"]["draft_var"] = json!("Draft Output");
        assert!(parse(&v, false).is_err());
    }
}

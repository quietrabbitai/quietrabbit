// src-tauri/src/conductor/model_block/draft.rs
//
// cb-12, the DRAFT DOCUMENT model block (decisions.id=941 point 2, decisions.id=942
// addendum 3). It expands into an ordinary Generate step.
//
// The skeleton, in order: the lead sentence, the latitude sentence (each latitude
// carries its own wording, the strict one including the plain-wording line), the
// output-only sentence, the type's format note and length target,
// the brief (one declared variable), then the sender details and the writer's
// background when the document type declares them. Every sentence is data
// (document_types.json, phrases.draft). Polish belongs to the voice block, so
// a strict draft asks for none; the framed and free phrases say nothing about it.
//
// Inputs are NAMED, not read from a store: brief_var holds the rendered
// structured brief (brief.rs), sender_var the sender details the type declares,
// context_var the personal background the type declares. Until the B7a seed
// exists a local step receives no personal background: a step's template reaches
// personal fields only through the disclosure buffer at tier 2 and above, and
// {persona_context} is one unfiltered blob this block never names.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::model_block::document_type::{fill, registry};
use crate::conductor::model_block::{
    check_var, finish, parse_row, resolve_type, BlockSpec, Built, ExpandInput, ParsedModelBlock,
};
use crate::conductor::tokens::{ActionDefinition, StepType};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftSpec {
    /// A document type id in the registry.
    pub document_type: String,
    /// The declared input holding the structured brief, as rendered text.
    pub brief_var: String,
    /// The declared input holding the sender details. Required exactly when the
    /// document type declares sender fields.
    pub sender_var: Option<String>,
    /// The declared input holding the personal background. Required exactly when
    /// the document type declares context fields.
    pub context_var: Option<String>,
}

pub(crate) fn parse(value: &Value, external_allowed: bool) -> Result<ParsedModelBlock, String> {
    let (common, spec): (_, DraftSpec) = parse_row("cb-12", value, external_allowed)?;
    check_var("cb-12", "brief_var", &spec.brief_var)?;
    for (what, v) in [
        ("sender_var", &spec.sender_var),
        ("context_var", &spec.context_var),
    ] {
        if let Some(v) = v {
            check_var("cb-12", what, v)?;
        }
    }
    let t = registry()?
        .document_type(&spec.document_type)
        .ok_or_else(|| format!("cb-12: unknown document_type '{}'", spec.document_type))?;
    if t.sender.is_empty() != spec.sender_var.is_none() {
        return Err(format!(
            "cb-12: document type '{}' {} sender details, so sender_var must be {}",
            t.id,
            if t.sender.is_empty() {
                "declares no"
            } else {
                "declares"
            },
            if t.sender.is_empty() { "null" } else { "set" }
        ));
    }
    if t.context_fields.is_empty() != spec.context_var.is_none() {
        return Err(format!(
            "cb-12: document type '{}' {} context fields, so context_var must be {}",
            t.id,
            if t.context_fields.is_empty() {
                "declares no"
            } else {
                "declares"
            },
            if t.context_fields.is_empty() {
                "null"
            } else {
                "set"
            }
        ));
    }
    Ok(ParsedModelBlock {
        common,
        spec: BlockSpec::Draft(spec),
    })
}

pub(crate) fn expand(
    parsed: &ParsedModelBlock,
    input: &ExpandInput,
) -> Result<ActionDefinition, String> {
    let BlockSpec::Draft(spec) = &parsed.spec else {
        return Err("cb-12: expand called on another block's row".to_owned());
    };
    let reg = registry()?;
    let (t, settings) = resolve_type(&spec.document_type, input, reg)?;
    let p = &reg.phrases.draft;
    let length = *t.length_words.get(&settings.length).ok_or_else(|| {
        format!(
            "document type '{}': no length entry for '{}'",
            t.id, settings.length
        )
    })?;

    let mut lines: Vec<String> = vec![
        fill(&p.lead, &[("form", &t.form)]),
        p.latitude(settings.latitude).to_owned(),
        fill(&p.output_only, &[("form", &t.form)]),
        String::new(),
        format!("{} {}", p.format_label, t.format_note),
        fill(
            &p.length,
            &[
                ("min_words", &length.min_words.to_string()),
                ("max_words", &length.max_words.to_string()),
            ],
        ),
        String::new(),
        p.brief_label.clone(),
        format!("{{{}}}", spec.brief_var),
    ];
    if let Some(v) = &spec.sender_var {
        lines.extend([String::new(), p.sender_label.clone(), format!("{{{v}}}")]);
    }
    if let Some(v) = &spec.context_var {
        lines.extend([String::new(), p.context_label.clone(), format!("{{{v}}}")]);
    }

    finish(
        "cb-12",
        parsed,
        input,
        Built {
            step_type: StepType::Generate,
            template: lines.join("\n"),
            params: t.params.draft,
            length,
        },
    )
}

pub(crate) fn example() -> Value {
    json!({
        "common": {
            "display_name": "Writing your letter...",
            "output_var": "draft_output",
            "need": { "task_type": "creative_writing", "difficulty": "low", "output_size": "short" },
            "declared_inputs": ["content_brief", "sender_details"],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "rma_letter",
            "brief_var": "content_brief",
            "sender_var": "sender_details",
            "context_var": null
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tokens::ExternalAccess;

    /// The worked example of the plan: a framed cover letter.
    pub(crate) fn cover_letter() -> Value {
        json!({
            "common": {
                "display_name": "Writing your cover letter...",
                "output_var": "draft_output",
                "need": { "task_type": "creative_writing", "difficulty": "medium", "output_size": "medium" },
                "declared_inputs": ["content_brief", "sender_details", "writer_context"],
                "range": {
                    "quality_floor": { "min": "basic", "max": "high" },
                    "highest_tier": "local_only",
                    "destinations": ["local"]
                },
                "class_preference": "medium"
            },
            "spec": {
                "document_type": "cover_letter",
                "brief_var": "content_brief",
                "sender_var": "sender_details",
                "context_var": "writer_context"
            }
        })
    }

    fn expand_json(
        v: &Value,
        external_allowed: bool,
        ceiling: ExternalAccess,
    ) -> Result<ActionDefinition, String> {
        let parsed = parse(v, external_allowed)?;
        expand(
            &parsed,
            &ExpandInput {
                action_id: "draft_writing",
                focus_ceiling: ceiling,
                settings: None,
                external_allowed,
            },
        )
    }

    #[test]
    fn strict_rma_letter_expands_to_an_ordinary_local_step() {
        let step = expand_json(&example(), false, ExternalAccess::AnonymousRequired).unwrap();
        assert_eq!(step.block_stable_id, None);
        assert!(step.block_customization.is_none());
        assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::LocalOnly)
        );
        assert!(!step.requires_user_handoff);
        assert_eq!(step.step_type, StepType::Generate);
        assert_eq!(step.task_type, "creative_writing");
        assert_eq!(step.output_var.as_deref(), Some("draft_output"));
        assert_eq!(
            step.declared_inputs,
            vec!["content_brief", "sender_details"]
        );
        let req = step.requirement.as_ref().expect("requirement");
        assert_eq!(req.block_stable_id, "cb-12");

        let t = &step.prompt_template;
        assert!(
            t.starts_with("Write a plain, complete letter from the points below."),
            "{t}"
        );
        assert!(t.contains("Use only the points and facts supplied here."));
        assert!(t.contains("Output only the letter itself"));
        assert!(t.contains("Aim for between 150 and 250 words."));
        assert!(t.contains("{content_brief}") && t.contains("{sender_details}"));
        assert!(!t.contains("{user_input}") && !t.contains("{persona_context}"));
        // Strict: no framing allowance, no invention.
        assert!(!t.contains("frame the supplied points"));
        assert!(!t.contains("invent"));

        assert_eq!(step.options_override["temperature"], json!(0.2));
        assert_eq!(step.options_override["num_predict"], json!(640));
        assert!(!step.options_override.contains_key("top_p"));
    }

    #[test]
    fn framed_cover_letter_uses_the_framed_sentence_and_names_the_context() {
        let step = expand_json(&cover_letter(), false, ExternalAccess::AnonymousRequired).unwrap();
        let t = &step.prompt_template;
        assert!(
            t.starts_with("Write a plain, complete cover letter from the points below."),
            "{t}"
        );
        assert!(
            t.contains("You may frame the supplied points persuasively"),
            "{t}"
        );
        assert!(t.contains("Do not add any new fact"));
        assert!(!t.contains("Use only the points and facts supplied here."));
        assert!(t.contains("{writer_context}"));
        assert_eq!(
            step.declared_inputs,
            vec!["content_brief", "sender_details", "writer_context"]
        );
        assert_eq!(
            step.requirement.as_ref().unwrap().class_preference,
            Some(crate::conductor::model_block::config::ModelClass::Medium)
        );
        assert_eq!(step.options_override["temperature"], json!(0.4));
    }

    #[test]
    fn the_wording_comes_from_the_data_file() {
        // Every sentence of the strict letter is a phrase of the registry: edit the
        // data file and the template changes with no code change.
        let reg = registry().unwrap();
        let p = &reg.phrases.draft;
        let step = expand_json(&example(), false, ExternalAccess::LocalOnly).unwrap();
        for phrase in [
            &p.latitude_strict,
            &p.format_label,
            &p.brief_label,
            &p.sender_label,
        ] {
            assert!(step.prompt_template.contains(phrase.as_str()), "{phrase}");
        }
    }

    #[test]
    fn the_plain_wording_line_belongs_to_the_strict_latitude_phrase_alone() {
        let reg = registry().unwrap();
        let p = &reg.phrases.draft;
        let polish = "Do not try to make it polished or impressive";
        assert!(p.latitude_strict.contains(polish));
        assert!(!p.latitude_framed.contains(polish));
        assert!(!p.latitude_free.contains(polish));
        // Code adds no separate line: the template carries it exactly when the
        // latitude phrase does.
        let strict = expand_json(&example(), false, ExternalAccess::LocalOnly).unwrap();
        assert!(strict.prompt_template.contains(polish));
        assert_eq!(strict.prompt_template.matches(polish).count(), 1);
        let framed = expand_json(&cover_letter(), false, ExternalAccess::LocalOnly).unwrap();
        assert!(!framed.prompt_template.contains(polish));
        let mut free = example();
        free["spec"]["document_type"] = json!("creative_story");
        free["spec"]["sender_var"] = Value::Null;
        free["common"]["declared_inputs"] = json!(["content_brief"]);
        let free = expand_json(&free, false, ExternalAccess::LocalOnly).unwrap();
        assert!(!free.prompt_template.contains(polish));
        assert!(free
            .prompt_template
            .contains("invent whatever the piece needs"));
    }

    #[test]
    fn sender_and_context_vars_must_match_what_the_type_declares() {
        // RMA declares sender fields and no context fields.
        let mut v = example();
        v["spec"]["sender_var"] = Value::Null;
        assert!(parse(&v, false)
            .unwrap_err()
            .contains("declares sender details"));
        let mut v = example();
        v["spec"]["context_var"] = json!("writer_context");
        v["common"]["declared_inputs"] =
            json!(["content_brief", "sender_details", "writer_context"]);
        assert!(parse(&v, false)
            .unwrap_err()
            .contains("declares no context fields"));
        // Cover letter declares both.
        let mut v = cover_letter();
        v["spec"]["context_var"] = Value::Null;
        assert!(parse(&v, false)
            .unwrap_err()
            .contains("context_var must be set"));
    }

    #[test]
    fn declared_inputs_must_be_exactly_what_the_template_names() {
        let mut v = example();
        v["common"]["declared_inputs"] = json!(["content_brief"]);
        assert!(expand_json(&v, false, ExternalAccess::LocalOnly)
            .unwrap_err()
            .contains("does not list it"));
        let mut v = example();
        v["common"]["declared_inputs"] = json!(["content_brief", "sender_details", "extra_var"]);
        assert!(expand_json(&v, false, ExternalAccess::LocalOnly)
            .unwrap_err()
            .contains("never used"));
    }

    #[test]
    fn unknown_document_type_and_unknown_keys_are_refused() {
        let mut v = example();
        v["spec"]["document_type"] = json!("haiku");
        assert!(parse(&v, false)
            .unwrap_err()
            .contains("unknown document_type 'haiku'"));
        let mut v = example();
        v["spec"]["step_type"] = json!("generate");
        assert!(parse(&v, false).unwrap_err().contains("unknown field"));
        let mut v = example();
        v["common"]["need"]["task_type"] = json!("poetry");
        assert!(parse(&v, false).unwrap_err().contains("poetry"));
        let mut v = example();
        v["common"]["model"] = json!("llama3.1:8b");
        assert!(parse(&v, false).unwrap_err().contains("unknown field"));
    }

    #[test]
    fn external_blocks_are_refused_while_the_constant_is_false() {
        let mut v = example();
        v["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        v["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        let e = parse(&v, false).unwrap_err();
        assert!(e.contains("not allowed yet"), "{e}");
        assert!(parse(&v, true).is_ok());
    }

    #[test]
    fn hosted_rule_a_hosted_block_names_neither_user_input_nor_persona_context() {
        let mut v = example();
        v["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        v["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        // Fine as authored: the brief and the sender details only.
        assert!(expand_json(&v, true, ExternalAccess::Unrestricted).is_ok());
        // A brief variable named user_input reaches the template: refused.
        v["spec"]["brief_var"] = json!("user_input");
        v["common"]["declared_inputs"] = json!(["user_input", "sender_details"]);
        let e = expand_json(&v, true, ExternalAccess::Unrestricted).unwrap_err();
        assert!(
            e.contains("{user_input}") && e.contains("hosted_api"),
            "{e}"
        );
        // So is persona_context, through the sender variable.
        let mut v = example();
        v["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        v["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        v["spec"]["sender_var"] = json!("persona_context");
        v["common"]["declared_inputs"] = json!(["content_brief", "persona_context"]);
        let e = expand_json(&v, true, ExternalAccess::Unrestricted).unwrap_err();
        assert!(e.contains("{persona_context}"), "{e}");
        // The same names are allowed in a block that cannot reach a hosted API.
        let mut v = example();
        v["spec"]["brief_var"] = json!("user_input");
        v["common"]["declared_inputs"] = json!(["user_input", "sender_details"]);
        assert!(expand_json(&v, false, ExternalAccess::LocalOnly).is_ok());
    }

    #[test]
    fn the_range_is_clamped_to_the_focus_ceiling_not_rejected() {
        // A hosted-range block in a Focus that only reaches local: narrowed to local.
        let mut v = example();
        v["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        v["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        let step = expand_json(&v, true, ExternalAccess::LocalOnly).unwrap();
        assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        let req = step.requirement.unwrap();
        assert_eq!(
            req.range.destinations,
            vec![crate::conductor::model_block::config::Destination::Local]
        );
        // With room, the block's own tier stands.
        let step = expand_json(&v, true, ExternalAccess::Unrestricted).unwrap();
        assert_eq!(step.routing_tier, ExternalAccess::AnonymousPreferred);
        assert_eq!(
            step.external_access_override,
            Some(ExternalAccess::AnonymousPreferred)
        );
    }

    #[test]
    fn settings_in_force_change_the_expansion() {
        let reg = registry().unwrap();
        let parsed = parse(&example(), false).unwrap();
        let mut s = reg.document_type("rma_letter").unwrap().settings.clone();
        s.latitude = crate::conductor::model_block::document_type::Latitude::Free;
        s.length = "short".to_owned();
        let step = expand(
            &parsed,
            &ExpandInput {
                action_id: "draft_writing",
                focus_ceiling: ExternalAccess::LocalOnly,
                settings: Some(&s),
                external_allowed: false,
            },
        )
        .unwrap();
        assert!(step
            .prompt_template
            .contains("invent whatever the piece needs"));
        assert!(step.prompt_template.contains("between 80 and 150 words"));
        assert_eq!(step.options_override["num_predict"], json!(400));
        // An unknown step in the settings is an error.
        s.tone = "sarcastic".to_owned();
        assert!(expand(
            &parsed,
            &ExpandInput {
                action_id: "draft_writing",
                focus_ceiling: ExternalAccess::LocalOnly,
                settings: Some(&s),
                external_allowed: false,
            },
        )
        .is_err());
    }
}

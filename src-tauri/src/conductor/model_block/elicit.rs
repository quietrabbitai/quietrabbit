// src-tauri/src/conductor/model_block/elicit.rs
//
// pc-03, the ELICIT model block, in its minimal ONE-ROUND form (items.id=903,
// decisions.id=963 ruling 4). It expands into an ordinary Generate step that turns
// the user's request into a structured brief (brief.rs, StructuredBrief) written as
// one JSON object, which the brief_put store operation validates, renders and stores
// as the content_brief artifact. Every sentence is data (document_types.json,
// phrases.elicit), with no brace in it, so the JSON shape is described in words.
//
// SHAPED FOR items.id=906, which extends it to multi-turn elicitation-to-completion
// (follow-up questions until the brief is complete, Library sources): `rounds` is a
// closed enum with the one value single today, so a second mode is an added variant
// with its own expansion and the configuration stays typed. The output contract (one
// StructuredBrief JSON object) is the seam both forms share with brief_put.
//
// The input is the user's own request (input_var, normally user_input), so the hosted
// rule in finish() keeps this block local: a range that allows hosted_api may not name
// {user_input}. While EXTERNAL_MODEL_BLOCKS_ALLOWED is false nothing else loads anyway.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::model_block::document_type::{registry, StepParams, WordRange};
use crate::conductor::model_block::{
    check_var, finish, parse_row, resolve_type, BlockSpec, Built, ExpandInput, ParsedModelBlock,
};
use crate::conductor::tokens::{ActionDefinition, StepType};

/// How the block elicits. One round only until items.id=906.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rounds {
    /// The user's one request becomes the brief.
    Single,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitSpec {
    /// A document type id in the registry: the brief is for this form of document.
    pub document_type: String,
    pub rounds: Rounds,
    /// The declared input holding the user's request.
    pub input_var: String,
}

/// A low temperature: the brief restates the request, it invents nothing. Author
/// data of the block, not a user setting.
const ELICIT_PARAMS: StepParams = StepParams {
    temperature: 0.1,
    top_p: None,
};

/// The brief is short: up to MAX_POINTS points of a sentence or two.
const ELICIT_LENGTH: WordRange = WordRange {
    min_words: 10,
    max_words: 600,
    num_predict: 900,
};

pub(crate) fn parse(value: &Value, external_allowed: bool) -> Result<ParsedModelBlock, String> {
    let (common, spec): (_, ElicitSpec) = parse_row("pc-03", value, external_allowed)?;
    check_var("pc-03", "input_var", &spec.input_var)?;
    if registry()?.document_type(&spec.document_type).is_none() {
        return Err(format!(
            "pc-03: unknown document_type '{}'",
            spec.document_type
        ));
    }
    Ok(ParsedModelBlock {
        common,
        spec: BlockSpec::Elicit(spec),
    })
}

pub(crate) fn expand(
    parsed: &ParsedModelBlock,
    input: &ExpandInput,
) -> Result<ActionDefinition, String> {
    let BlockSpec::Elicit(spec) = &parsed.spec else {
        return Err("pc-03: expand called on another block's row".to_owned());
    };
    let reg = registry()?;
    let (t, _settings) = resolve_type(&spec.document_type, input, reg)?;
    let p = &reg.phrases.elicit;
    let lines = [
        crate::conductor::model_block::document_type::fill(&p.lead, &[("form", &t.form)]),
        p.shape.clone(),
        p.only_supplied.clone(),
        p.output_only.clone(),
        String::new(),
        p.request_label.clone(),
        format!("{{{}}}", spec.input_var),
    ];
    let Rounds::Single = spec.rounds;
    finish(
        "pc-03",
        parsed,
        input,
        Built {
            step_type: StepType::Generate,
            template: lines.join("\n"),
            params: ELICIT_PARAMS,
            length: ELICIT_LENGTH,
            // The user's request is a system token, never an absent bound name.
            required: vec![spec.input_var.clone()],
        },
    )
}

pub(crate) fn example() -> Value {
    json!({
        "common": {
            "display_name": "Reading your request...",
            "output_var": "brief_json",
            "need": { "task_type": "quick_response", "difficulty": "low", "output_size": "short" },
            "declared_inputs": ["user_input"],
            "range": {
                "quality_floor": { "min": "basic", "max": "standard" },
                "highest_tier": "local_only",
                "destinations": ["local"]
            },
            "class_preference": null
        },
        "spec": {
            "document_type": "personal_note",
            "rounds": "single",
            "input_var": "user_input"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::ExpandInput;
    use crate::conductor::tokens::ExternalAccess;

    fn expand_example(v: &Value) -> Result<ActionDefinition, String> {
        let parsed = parse(v, false)?;
        expand(
            &parsed,
            &ExpandInput {
                action_id: "brief",
                focus_ceiling: ExternalAccess::AnonymousRequired,
                settings: None,
                external_allowed: false,
                document_type: None,
                envelope_override: None,
            },
        )
    }

    #[test]
    fn the_example_expands_to_a_local_step_naming_only_the_request() {
        let s = expand_example(&example()).unwrap();
        assert_eq!(s.output_var.as_deref(), Some("brief_json"));
        assert_eq!(s.routing_tier, ExternalAccess::LocalOnly);
        assert_eq!(
            crate::conductor::executor::find_tokens(&s.prompt_template),
            ["user_input"]
        );
        assert!(s.prompt_template.contains("note"), "the form is filled in");
        // user_input is a system token, so the block emits no guard of its own.
        assert!(s.and_guards.is_empty());
        assert_eq!(s.task_type, "quick_response");
    }

    #[test]
    fn the_configuration_is_closed() {
        let mut v = example();
        v["spec"]["rounds"] = json!("until_complete");
        assert!(
            parse(&v, false).is_err(),
            "only single exists until items.id=906"
        );
        let mut v = example();
        v["spec"]["sources"] = json!([]);
        assert!(parse(&v, false).is_err());
        let mut v = example();
        v["spec"]["document_type"] = json!("sonnet");
        assert!(parse(&v, false)
            .unwrap_err()
            .contains("unknown document_type"));
    }

    #[test]
    fn a_hosted_range_naming_the_request_is_refused() {
        let mut v = example();
        v["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        v["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        let parsed = parse(&v, true).unwrap();
        let e = expand(
            &parsed,
            &ExpandInput {
                action_id: "brief",
                focus_ceiling: ExternalAccess::AnonymousRequired,
                settings: None,
                external_allowed: true,
                document_type: None,
                envelope_override: None,
            },
        )
        .unwrap_err();
        assert!(e.contains("user_input"), "{e}");
    }
}

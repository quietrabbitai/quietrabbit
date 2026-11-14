// src-tauri/src/conductor/model_block/brief.rs
//
// items.id=894 part 1 (decisions.id=942 addendum 3 a, e): the STRUCTURED BRIEF,
// the draft block's input. A list of points and facts, not prose: the calibration
// variant (examples/focus_calibration.rs, items.id=894 part 0) showed that handing
// a model a structured list of points, with the task and recipient named, is what
// removed placeholders and chatter in the bundled models.
//
// The draft block consumes the brief as ONE declared variable holding rendered
// text (DraftSpec.brief_var). Whatever produces the variable (the brief artifact
// of B3a, an elicitation pass of B7b) renders it with render() so the layout is
// the one the calibration measured. The labels are data (document_types.json,
// phrases.brief). This module is pure and holds no personal data.

use serde::{Deserialize, Serialize};

use crate::conductor::model_block::document_type::BriefPhrases;

/// Most points a brief may carry, and the longest one point or the task may be.
pub const MAX_POINTS: usize = 40;
pub const MAX_TEXT_CHARS: usize = 600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StructuredBrief {
    /// What the document is for, in a short phrase.
    pub task: String,
    /// Who it is for, when that matters.
    pub recipient: Option<String>,
    /// The points and facts to cover, one per entry.
    pub points: Vec<String>,
}

impl StructuredBrief {
    pub fn validate(&self) -> Result<(), String> {
        let check = |what: &str, s: &str| -> Result<(), String> {
            if s.trim().is_empty() {
                return Err(format!("brief: {what} is empty"));
            }
            if s.chars().count() > MAX_TEXT_CHARS {
                return Err(format!(
                    "brief: {what} is longer than {MAX_TEXT_CHARS} characters"
                ));
            }
            Ok(())
        };
        check("task", &self.task)?;
        if let Some(r) = &self.recipient {
            check("recipient", r)?;
        }
        if self.points.is_empty() {
            return Err("brief: no points".to_owned());
        }
        if self.points.len() > MAX_POINTS {
            return Err(format!("brief: more than {MAX_POINTS} points"));
        }
        for (i, p) in self.points.iter().enumerate() {
            check(&format!("point {}", i + 1), p)?;
        }
        Ok(())
    }

    /// The text a draft step reads. Deterministic: task, recipient (when
    /// present), then one bullet per point.
    pub fn render(&self, labels: &BriefPhrases) -> String {
        let mut out = format!("{} {}\n", labels.task_label, self.task.trim());
        if let Some(r) = &self.recipient {
            out.push_str(&format!("{} {}\n", labels.recipient_label, r.trim()));
        }
        out.push('\n');
        out.push_str(&labels.points_label);
        for p in &self.points {
            out.push_str("\n- ");
            out.push_str(p.trim());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::document_type::registry;

    fn brief() -> StructuredBrief {
        StructuredBrief {
            task: "Ask for a refund for a faulty drill".to_owned(),
            recipient: Some("Returns department".to_owned()),
            points: vec![
                "Bought on 2 November".to_owned(),
                "The drill stopped working after two days".to_owned(),
            ],
        }
    }

    #[test]
    fn render_is_deterministic_and_lists_every_point() {
        let labels = &registry().unwrap().phrases.brief;
        let text = brief().render(labels);
        assert_eq!(
            text,
            "Task: Ask for a refund for a faulty drill\n\
             Recipient: Returns department\n\
             \n\
             Points and facts to cover:\n\
             - Bought on 2 November\n\
             - The drill stopped working after two days"
        );
        assert_eq!(text, brief().render(labels));
    }

    #[test]
    fn a_brief_without_a_recipient_omits_the_line() {
        let labels = &registry().unwrap().phrases.brief;
        let mut b = brief();
        b.recipient = None;
        assert!(!b.render(labels).contains("Recipient"));
    }

    #[test]
    fn validation_rejects_empty_and_oversized() {
        assert!(brief().validate().is_ok());
        let mut b = brief();
        b.points.clear();
        assert!(b.validate().is_err());
        let mut b = brief();
        b.task = "  ".to_owned();
        assert!(b.validate().is_err());
        let mut b = brief();
        b.points = (0..=MAX_POINTS).map(|i| format!("p{i}")).collect();
        assert!(b.validate().is_err());
        let mut b = brief();
        b.points[0] = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(b.validate().is_err());
        assert!(
            serde_json::from_str::<StructuredBrief>(r#"{"task":"t","points":["a"],"x":1}"#)
                .is_err()
        );
    }
}

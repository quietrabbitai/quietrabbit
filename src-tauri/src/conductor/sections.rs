// src-tauri/src/conductor/sections.rs
//
// items.id=903 (decisions.id=942 point 6): SECTION-BY-SECTION passes. Pure: no I/O.
// seeding.rs computes three DERIVED bound names from two artifacts at pass start:
//   sections_state   none (no stored brief, or a brief with no points), more (a point
//                    has no section in the working draft yet) or done (every point has
//                    one). A closed literal set, so guards enumerate it exactly;
//   section_heading  the first point with no section yet (empty unless more);
//   next_section     what the section's draft step reads: the stored brief, then the
//                    data sentence naming the one section to write now (empty unless
//                    more). Capped like every derived text (DERIVED_TEXT_MAX_BYTES).
// Each point of the brief is one section. A section is WRITTEN when the working draft
// has a heading line `## <point>` (artifact_append writes exactly that). Matching is on
// the trimmed text, case-insensitively. The brief and the draft are the seeded copies
// (braces already neutralized), so a heading compares as stored.

/// The literals of sections_state.
pub const SECTIONS_NONE: &str = "none";
pub const SECTIONS_MORE: &str = "more";
pub const SECTIONS_DONE: &str = "done";
pub const SECTIONS_STATE_VALUES: &[&str] = &[SECTIONS_NONE, SECTIONS_MORE, SECTIONS_DONE];

/// The three values for one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionSeeds {
    pub state: &'static str,
    pub section_heading: String,
    pub next_section: String,
}

/// The brief's points: its bullet lines (brief::render writes one `- ` line per point).
pub fn brief_points(brief: &str) -> Vec<String> {
    brief
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The section headings already in the draft (`## ` lines).
pub fn written_headings(draft: &str) -> Vec<String> {
    draft
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("## "))
        .map(|h| h.trim().to_lowercase())
        .collect()
}

/// The section seeds. `section_label` is the data sentence (phrases.draft.section_label).
pub fn compute(brief: Option<&str>, draft: Option<&str>, section_label: &str) -> SectionSeeds {
    let none = SectionSeeds {
        state: SECTIONS_NONE,
        section_heading: String::new(),
        next_section: String::new(),
    };
    let Some(brief) = brief else { return none };
    let points = brief_points(brief);
    if points.is_empty() {
        return none;
    }
    let written = written_headings(draft.unwrap_or(""));
    match points
        .iter()
        .find(|p| !written.contains(&p.chars().take(200).collect::<String>().to_lowercase()))
    {
        Some(point) => {
            let heading: String = point.chars().take(200).collect();
            SectionSeeds {
                state: SECTIONS_MORE,
                next_section: format!("{brief}\n\n{section_label} {heading}"),
                section_heading: heading,
            }
        }
        None => SectionSeeds {
            state: SECTIONS_DONE,
            section_heading: String::new(),
            next_section: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str =
        "Task: An essay\n\nPoints and facts to cover:\n- Background\n- Method\n- Results";
    const LABEL: &str = "Write only this section:";

    #[test]
    fn no_brief_or_no_points_is_none() {
        assert_eq!(compute(None, None, LABEL).state, SECTIONS_NONE);
        assert_eq!(compute(Some("Task: x"), None, LABEL).state, SECTIONS_NONE);
    }

    #[test]
    fn sections_are_written_in_point_order_until_done() {
        let s = compute(Some(BRIEF), None, LABEL);
        assert_eq!(
            (s.state, s.section_heading.as_str()),
            (SECTIONS_MORE, "Background")
        );
        assert!(s.next_section.starts_with(BRIEF));
        assert!(s
            .next_section
            .ends_with("Write only this section: Background"));
        let draft = "## Background\n\nSome text.";
        let s = compute(Some(BRIEF), Some(draft), LABEL);
        assert_eq!(s.section_heading, "Method");
        // Case and surrounding space do not matter.
        let draft = "## background \n\nx\n\n##   METHOD\n\ny";
        assert_eq!(
            compute(Some(BRIEF), Some(draft), LABEL).section_heading,
            "Results"
        );
        let draft = "## Background\n\nx\n\n## Method\n\ny\n\n## Results\n\nz";
        let s = compute(Some(BRIEF), Some(draft), LABEL);
        assert_eq!(s.state, SECTIONS_DONE);
        assert!(s.section_heading.is_empty() && s.next_section.is_empty());
    }

    #[test]
    fn a_heading_inside_a_line_does_not_count() {
        let draft = "The ## Background";
        assert_eq!(
            compute(Some(BRIEF), Some(draft), LABEL).section_heading,
            "Background"
        );
    }
}

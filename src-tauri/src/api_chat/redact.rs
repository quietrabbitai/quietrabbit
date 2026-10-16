// src-tauri/src/api_chat/redact.rs
//
// Applies the user's Privacy Guardian decisions to the text of an API Chat
// message before it is sent (items.id=716). A Rust port of the front end's
// `applyCopyDecisions` (frontend/src/chat/copyDecisions.ts), with the same
// semantics, because here the server is the one that sends: the text a
// provider receives must reflect the user's choices, and the front end cannot
// be what enforces that.
//
//   release_original -> the original text stays
//   keep_private     -> "[removed]"
//   generalize       -> the user's edited text, else the suggestion, else "[removed]"
//   a decision string this code does not know -> keep_private (the most private)
//
// Offsets are BYTE offsets into the UTF-8 text, as gate3 emits them. Anything
// that cannot be applied safely (out of range, inverted, overlapping, not on a
// character boundary) returns None and the caller must NOT send: the original
// text is never the fallback.

/// Written in place of a kept-private span, or a generalize with no
/// replacement text. Matches the front end's COPY_REMOVED_PLACEHOLDER.
pub const REMOVED_PLACEHOLDER: &str = "[removed]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Generalize,
    KeepPrivate,
    ReleaseOriginal,
}

impl Decision {
    /// Anything unrecognised is the most private choice: a decision string
    /// this code does not understand must never release text.
    pub fn parse(s: &str) -> Self {
        match s {
            "generalize" => Self::Generalize,
            "release_original" => Self::ReleaseOriginal,
            _ => Self::KeepPrivate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanDecision {
    pub start_byte: usize,
    pub end_byte: usize,
    pub decision: Decision,
    pub suggestion_text: Option<String>,
    pub user_modified_text: Option<String>,
}

impl SpanDecision {
    /// A span nobody decided on: removed.
    pub fn unresolved(start_byte: usize, end_byte: usize) -> Self {
        Self {
            start_byte,
            end_byte,
            decision: Decision::KeepPrivate,
            suggestion_text: None,
            user_modified_text: None,
        }
    }

    /// None means "leave the original text in place".
    fn replacement(&self) -> Option<&str> {
        match self.decision {
            Decision::ReleaseOriginal => None,
            Decision::KeepPrivate => Some(REMOVED_PLACEHOLDER),
            Decision::Generalize => {
                let t = self
                    .user_modified_text
                    .as_deref()
                    .or(self.suggestion_text.as_deref())
                    .unwrap_or("")
                    .trim();
                Some(if t.is_empty() { REMOVED_PLACEHOLDER } else { t })
            }
        }
    }
}

/// The text to send, or None when the spans cannot be applied safely.
/// An empty span list returns the text unchanged.
pub fn apply(text: &str, mut spans: Vec<SpanDecision>) -> Option<String> {
    if spans.is_empty() {
        return Some(text.to_owned());
    }
    spans.sort_by_key(|s| s.start_byte);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for span in &spans {
        if span.start_byte < cursor
            || span.end_byte < span.start_byte
            || span.end_byte > text.len()
            || !text.is_char_boundary(span.start_byte)
            || !text.is_char_boundary(span.end_byte)
        {
            return None;
        }
        out.push_str(&text[cursor..span.start_byte]);
        match span.replacement() {
            Some(r) => out.push_str(r),
            None => out.push_str(&text[span.start_byte..span.end_byte]),
        }
        cursor = span.end_byte;
    }
    out.push_str(&text[cursor..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize, d: Decision) -> SpanDecision {
        SpanDecision {
            start_byte: start,
            end_byte: end,
            decision: d,
            suggestion_text: None,
            user_modified_text: None,
        }
    }

    #[test]
    fn no_spans_leaves_the_text_alone() {
        assert_eq!(apply("hello", vec![]).as_deref(), Some("hello"));
    }

    #[test]
    fn each_decision_has_its_replacement() {
        let text = "call Ann at 555-0100 now";
        let ann = (5, 8);
        let phone = (12, 20);
        let out = apply(
            text,
            vec![
                span(ann.0, ann.1, Decision::KeepPrivate),
                span(phone.0, phone.1, Decision::ReleaseOriginal),
            ],
        );
        assert_eq!(out.as_deref(), Some("call [removed] at 555-0100 now"));
    }

    #[test]
    fn generalize_prefers_the_users_edit_then_the_suggestion_then_removes() {
        let text = "ask Ann";
        let mut s = span(4, 7, Decision::Generalize);
        s.suggestion_text = Some("[person]".to_owned());
        assert_eq!(
            apply(text, vec![s.clone()]).as_deref(),
            Some("ask [person]")
        );
        s.user_modified_text = Some("  a friend ".to_owned());
        assert_eq!(
            apply(text, vec![s.clone()]).as_deref(),
            Some("ask a friend")
        );
        s.user_modified_text = Some("   ".to_owned());
        s.suggestion_text = None;
        assert_eq!(apply(text, vec![s]).as_deref(), Some("ask [removed]"));
        assert_eq!(
            apply(text, vec![span(4, 7, Decision::Generalize)]).as_deref(),
            Some("ask [removed]")
        );
    }

    #[test]
    fn an_unknown_decision_string_is_the_most_private_choice() {
        assert_eq!(Decision::parse("generalize"), Decision::Generalize);
        assert_eq!(
            Decision::parse("release_original"),
            Decision::ReleaseOriginal
        );
        assert_eq!(Decision::parse("keep_private"), Decision::KeepPrivate);
        assert_eq!(Decision::parse("approve-everything"), Decision::KeepPrivate);
        assert_eq!(Decision::parse(""), Decision::KeepPrivate);
    }

    #[test]
    fn an_undecided_span_is_removed() {
        let out = apply("hi Ann", vec![SpanDecision::unresolved(3, 6)]);
        assert_eq!(out.as_deref(), Some("hi [removed]"));
    }

    #[test]
    fn spans_are_applied_in_offset_order_whatever_order_they_arrive_in() {
        let text = "a Ann b Bob c";
        let out = apply(
            text,
            vec![
                span(8, 11, Decision::KeepPrivate),
                span(2, 5, Decision::KeepPrivate),
            ],
        );
        assert_eq!(out.as_deref(), Some("a [removed] b [removed] c"));
    }

    #[test]
    fn multi_byte_text_is_sliced_on_byte_offsets() {
        let text = "né Ann";
        // "né " is 4 bytes (n, é = 2 bytes, space); Ann is 4..7
        let out = apply(text, vec![span(4, 7, Decision::KeepPrivate)]);
        assert_eq!(out.as_deref(), Some("né [removed]"));
    }

    #[test]
    fn a_span_that_splits_a_character_cannot_be_applied() {
        // "é" occupies bytes 1..3; an offset of 2 is inside it.
        assert_eq!(apply("né", vec![span(2, 3, Decision::KeepPrivate)]), None);
    }

    #[test]
    fn overlapping_inverted_or_out_of_range_spans_cannot_be_applied() {
        let t = "hello world";
        assert_eq!(
            apply(
                t,
                vec![
                    span(0, 5, Decision::KeepPrivate),
                    span(3, 8, Decision::KeepPrivate)
                ]
            ),
            None,
            "overlap"
        );
        assert_eq!(
            apply(t, vec![span(5, 2, Decision::KeepPrivate)]),
            None,
            "inverted"
        );
        assert_eq!(
            apply(t, vec![span(0, 99, Decision::KeepPrivate)]),
            None,
            "past the end"
        );
    }
}

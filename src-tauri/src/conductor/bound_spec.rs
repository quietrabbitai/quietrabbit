// src-tauri/src/conductor/bound_spec.rs
//
// items.id=886 (decisions.id=939 section 2): the compiled spec table of the
// BOUND NAMES, the variables a pass reads from the session and case stores at
// pass start instead of from an earlier step. declared_inputs::bound_names()
// returns NAMES, and a test pins NAMES and SPECS together, so a name cannot be
// added to one and not the other.
//
// Each name has:
//   source    Session: a row of session_variables (persistence::session_store),
//             or the one-time write latch. Derived: computed read-only from the
//             case store (persistence::case_store) at pass start.
//   presence  Optional: the name is ABSENT until something writes it (a guard
//             on it is false, its token renders as nothing). Always: every pass
//             that asks for it gets a value, and a read that fails fails the
//             pass (seeding.rs), never a default.
//   literals  Closed: the value is always one of these literals, so guard path
//             enumeration at LOAD uses exactly them. Authored: the value space
//             is open (an intent, an id, a block of text), so the guards that
//             name it supply the literals, plus one implicit OTHER, exactly as
//             for a model-produced router variable.
//   validate  the shape a value must have. A value the store returns that fails
//             it fails the pass closed.
//
// write_mode is Session by source (the latch row lives in session_variables)
// and Always by presence: the latch row's absence IS the open state
// (session_store::write_mode), so a successful read always yields "open" or
// "suppressed", and the only way to not get a value is a read error, which
// fails the pass. It is never defaulted to open.
//
// NO NAME IS A SYSTEM TOKEN, an output_var or a name a Focus may declare for
// itself: LOAD rejects an output_var that equals one (declared_inputs.rs), and a
// test below pins the table against the five system tokens.

/// Where a bound name's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// session_variables (or the write latch): written by typed store writers.
    Session,
    /// Computed read-only from the case store at pass start.
    Derived,
}

/// Whether a pass can find the name absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Optional,
    Always,
}

/// The values a bound name can take, for guard path enumeration at LOAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Literals {
    Closed(&'static [&'static str]),
    Authored,
}

pub struct BoundSpec {
    pub name: &'static str,
    pub source: Source,
    pub presence: Presence,
    pub literals: Literals,
    pub validate: fn(&str) -> bool,
}

/// Longest value of a derived text name, in bytes. Longer text is cut at a
/// character boundary by seeding.rs.
pub const DERIVED_TEXT_MAX_BYTES: usize = 6000;

pub const WRITE_MODE: &str = "write_mode";
pub const LOCKED_INTENT: &str = "locked_intent";
pub const ACTIVE_CASE_ID: &str = "active_case_id";
pub const OPEN_TICKET_STATE: &str = "open_ticket_state";
pub const OPEN_TICKETS_BRIEF: &str = "open_tickets_brief";
pub const CASE_CONTEXT: &str = "case_context";
pub const DELTA_CONTEXT: &str = "delta_context";

pub const WRITE_MODE_VALUES: &[&str] = &["open", "suppressed"];
pub const OPEN_TICKET_STATE_VALUES: &[&str] = &["none", "one", "many"];

/// The names, in table order. declared_inputs::bound_names() returns this.
pub const NAMES: &[&str] = &[
    WRITE_MODE,
    LOCKED_INTENT,
    ACTIVE_CASE_ID,
    OPEN_TICKET_STATE,
    OPEN_TICKETS_BRIEF,
    CASE_CONTEXT,
    DELTA_CONTEXT,
];

pub const SPECS: &[BoundSpec] = &[
    BoundSpec {
        name: WRITE_MODE,
        source: Source::Session,
        presence: Presence::Always,
        literals: Literals::Closed(WRITE_MODE_VALUES),
        validate: valid_write_mode,
    },
    BoundSpec {
        name: LOCKED_INTENT,
        source: Source::Session,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_identifier,
    },
    BoundSpec {
        name: ACTIVE_CASE_ID,
        source: Source::Session,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_case_id,
    },
    BoundSpec {
        name: OPEN_TICKET_STATE,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Closed(OPEN_TICKET_STATE_VALUES),
        validate: valid_open_ticket_state,
    },
    BoundSpec {
        name: OPEN_TICKETS_BRIEF,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_text,
    },
    BoundSpec {
        name: CASE_CONTEXT,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_text,
    },
    BoundSpec {
        name: DELTA_CONTEXT,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_text,
    },
];

/// The spec of a bound name, or None for any other name.
pub fn spec(name: &str) -> Option<&'static BoundSpec> {
    SPECS.iter().find(|s| s.name == name)
}

fn valid_write_mode(s: &str) -> bool {
    WRITE_MODE_VALUES.contains(&s)
}

fn valid_open_ticket_state(s: &str) -> bool {
    OPEN_TICKET_STATE_VALUES.contains(&s)
}

/// An intent name: lowercase letters, digits and underscores, starting with a
/// letter, at most 64 characters (the same shape as a session variable name).
fn valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A case id: 1 to 128 characters of letters, digits and . _ : -
fn valid_case_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Free text (derived names): bounded and without a NUL. Empty is valid: a
/// derived name always exists and says "nothing" with an empty value.
fn valid_text(s: &str) -> bool {
    s.len() <= DERIVED_TEXT_MAX_BYTES && !s.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tokens::is_system_token;

    #[test]
    fn names_and_specs_are_one_table() {
        let from_specs: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
        assert_eq!(from_specs, NAMES, "NAMES must list SPECS in table order");
        assert_eq!(
            crate::conductor::declared_inputs::bound_names(),
            NAMES,
            "bound_names() must return the spec table's names"
        );
        let mut sorted = from_specs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), NAMES.len(), "a name appears twice");
    }

    /// The table as designed, pinned: a change here is a design change.
    #[test]
    fn the_table_is_pinned() {
        let row = |n: &str| {
            let s = spec(n).unwrap();
            (
                s.source,
                s.presence,
                match s.literals {
                    Literals::Closed(l) => Some(l),
                    Literals::Authored => None,
                },
            )
        };
        assert_eq!(
            row(WRITE_MODE),
            (Source::Session, Presence::Always, Some(WRITE_MODE_VALUES))
        );
        assert_eq!(
            row(LOCKED_INTENT),
            (Source::Session, Presence::Optional, None)
        );
        assert_eq!(
            row(ACTIVE_CASE_ID),
            (Source::Session, Presence::Optional, None)
        );
        assert_eq!(
            row(OPEN_TICKET_STATE),
            (
                Source::Derived,
                Presence::Always,
                Some(OPEN_TICKET_STATE_VALUES)
            )
        );
        for n in [OPEN_TICKETS_BRIEF, CASE_CONTEXT, DELTA_CONTEXT] {
            assert_eq!(row(n), (Source::Derived, Presence::Always, None), "{n}");
        }
        assert_eq!(WRITE_MODE_VALUES, &["open", "suppressed"]);
        assert_eq!(OPEN_TICKET_STATE_VALUES, &["none", "one", "many"]);
        // A derived name is never absent.
        for s in SPECS.iter().filter(|s| s.source == Source::Derived) {
            assert_eq!(s.presence, Presence::Always, "{}", s.name);
        }
    }

    #[test]
    fn no_bound_name_is_a_system_token_and_every_name_is_a_valid_token() {
        for n in NAMES {
            assert!(!is_system_token(n), "{n} is a system token");
            assert_eq!(
                crate::conductor::executor::find_tokens(&format!("{{{n}}}")),
                [(*n).to_owned()],
                "{n} is not a name find_tokens sees"
            );
        }
    }

    #[test]
    fn closed_literals_satisfy_their_own_validator() {
        for s in SPECS {
            if let Literals::Closed(lits) = s.literals {
                for l in lits {
                    assert!((s.validate)(l), "{}: '{l}'", s.name);
                }
                assert!(!(s.validate)("not-a-literal"), "{}", s.name);
            }
        }
    }

    #[test]
    fn validators_reject_what_they_should() {
        assert!(valid_identifier("billing_issue2"));
        assert!(!valid_identifier("Billing"));
        assert!(!valid_identifier("2fast"));
        assert!(!valid_identifier(""));
        assert!(!valid_identifier(&"a".repeat(65)));
        assert!(valid_case_id("case-1.a:b_c"));
        assert!(!valid_case_id("has space"));
        assert!(!valid_case_id("{brace}"));
        assert!(!valid_case_id(""));
        assert!(!valid_case_id(&"a".repeat(129)));
        assert!(valid_text(""));
        assert!(!valid_text("a\0b"));
        assert!(!valid_text(&"a".repeat(DERIVED_TEXT_MAX_BYTES + 1)));
    }
}

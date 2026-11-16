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
//             case store (persistence::case_store) at pass start. Artifact
//             (items.id=895): the current row of one kind in pass_artifacts
//             (persistence::pass_artifact_store), written only by the
//             artifact_put and artifact_clear store operations and absent
//             until the first write. Launch (items.id=897): the answer of a
//             per-Focus launch question, or its default, resolved ONCE per run at
//             LOAD (conductor::run_settings) and seeded from that resolution.
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
    /// items.id=895: the current per-pass artifact of one kind
    /// (pass_artifacts, persistence::pass_artifact_store). Read-only to a
    /// template, a guard and a seed: only artifact_put and artifact_clear write
    /// it. Never an argument of a store writer (a stored artifact is not data to
    /// copy back) and never a session_put target.
    Artifact,
    /// items.id=897 (decisions.id=948 point 2): a LAUNCH name. Its answers are
    /// stored as session variables of the chat (written only by the typed launch-
    /// answer store, never by a Focus row), but a run reads them ONCE, at LOAD, and
    /// every pass seeds the resolved value, so the re-expanded template and the
    /// seeds cannot disagree if an answer changes mid-run. Read-only to a template,
    /// a guard and a seed: never a session_put target, a writer's argument or a
    /// row's output_var.
    Launch,
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
/// items.id=891: the active case's primary device and what is recorded about it
/// (conductor::device_seeding). Read from personal.db (the entity and its facts)
/// and outputs.db (the active case). Empty when there is no active case or its
/// device is not this Focus's own active one.
pub const SUBJECT_CONTEXT: &str = "subject_context";
/// items.id=891: one line per active device the Focus owns, newest first, each
/// opening with the E-XXXXXXXX handle a model can name (conductor::device_seeding).
pub const DEVICE_LIBRARY_BRIEF: &str = "device_library_brief";

/// items.id=897 (decisions.id=948 point 2): the six LAUNCH names. document_type
/// and launch_mode are role names, launch_answer_1 to 3 are the generic slots a
/// Focus's other questions use, and run_change is the per-run settings patch (the
/// third layer of decisions.id=942 addendum 3 g). All are Source::Launch. The slot
/// names are exactly launch_config::SLOTS (a test pins it).
pub const DOCUMENT_TYPE: &str = "document_type";
pub const LAUNCH_MODE: &str = "launch_mode";
pub const LAUNCH_ANSWER_1: &str = "launch_answer_1";
pub const LAUNCH_ANSWER_2: &str = "launch_answer_2";
pub const LAUNCH_ANSWER_3: &str = "launch_answer_3";
pub const RUN_CHANGE: &str = "run_change";

/// The names a launch QUESTION can own, in table order (a run_change is not a
/// question).
pub const LAUNCH_SLOT_NAMES: [&str; 5] = [
    DOCUMENT_TYPE,
    LAUNCH_MODE,
    LAUNCH_ANSWER_1,
    LAUNCH_ANSWER_2,
    LAUNCH_ANSWER_3,
];

/// Every launch name, in table order.
pub const LAUNCH_NAMES: [&str; 6] = [
    DOCUMENT_TYPE,
    LAUNCH_MODE,
    LAUNCH_ANSWER_1,
    LAUNCH_ANSWER_2,
    LAUNCH_ANSWER_3,
    RUN_CHANGE,
];

/// Longest launch answer (a question or option id): the shape every slot value
/// has. The submit command checks membership in the question's options.
pub const LAUNCH_ANSWER_MAX_CHARS: usize = 40;
/// Longest run_change value: the session store's own limit.
pub const RUN_CHANGE_MAX_BYTES: usize = 2048;

/// items.id=895: the four per-pass artifact names. Each equals its
/// pass_artifact_store::ArtifactKind::as_str() (a test pins it), so a name is
/// reserved for the artifact and no row's output_var may use it (the
/// declared-inputs lint rejects that): a producing step uses another variable
/// and an artifact_put row stores it under the name.
pub const CONTENT_BRIEF: &str = "content_brief";
pub const VOICE_GUIDANCE: &str = "voice_guidance";
pub const VALIDATION_REPORT: &str = "validation_report";
pub const DELTA_ITEMS: &str = "delta_items";

/// items.id=387: the two session names of a manual hand-off. handoff_pending holds
/// the opaque id of a hand-off whose starter was rendered and whose reply has not
/// come back (`h_` and 12 lowercase hex characters). handoff_received holds the H-
/// handle of the reply stored by handoff_ingest (persistence::handles, Kind::Draft).
/// They are written only by the hand-off operations (the outbound render, S3, and
/// handoff_ingest): LOAD refuses them as a session_put target and as the argument of
/// any writer. Both are ordinary Session-source bound names otherwise: restricted to
/// the step whose own template names them, tainting such a step, and (since items.id=884
/// flipped STORE_NAMES_EXTERNAL_ALLOWED) allowed in an external step that declares
/// them, behind the outbound review. Their normal use is a guard (`present`,
/// `absent`), so a pass routes on them.
pub const HANDOFF_PENDING: &str = "handoff_pending";
pub const HANDOFF_RECEIVED: &str = "handoff_received";

/// The hand-off names, in table order.
pub const HANDOFF_NAMES: [&str; 2] = [HANDOFF_PENDING, HANDOFF_RECEIVED];

/// The artifact names, in table order.
pub const ARTIFACT_NAMES: [&str; 4] = [
    CONTENT_BRIEF,
    VOICE_GUIDANCE,
    VALIDATION_REPORT,
    DELTA_ITEMS,
];

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
    SUBJECT_CONTEXT,
    DEVICE_LIBRARY_BRIEF,
    CONTENT_BRIEF,
    VOICE_GUIDANCE,
    VALIDATION_REPORT,
    DELTA_ITEMS,
    HANDOFF_PENDING,
    HANDOFF_RECEIVED,
    DOCUMENT_TYPE,
    LAUNCH_MODE,
    LAUNCH_ANSWER_1,
    LAUNCH_ANSWER_2,
    LAUNCH_ANSWER_3,
    RUN_CHANGE,
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
    BoundSpec {
        name: SUBJECT_CONTEXT,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_text,
    },
    BoundSpec {
        name: DEVICE_LIBRARY_BRIEF,
        source: Source::Derived,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_text,
    },
    // items.id=895: the artifacts. Optional (absent until written, so a guard on
    // a name is false and its token renders as nothing) and Authored (open text).
    BoundSpec {
        name: CONTENT_BRIEF,
        source: Source::Artifact,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_artifact,
    },
    BoundSpec {
        name: VOICE_GUIDANCE,
        source: Source::Artifact,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_artifact,
    },
    BoundSpec {
        name: VALIDATION_REPORT,
        source: Source::Artifact,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_artifact,
    },
    BoundSpec {
        name: DELTA_ITEMS,
        source: Source::Artifact,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_artifact,
    },
    // items.id=387: the hand-off names. Optional (absent until a hand-off starts, and
    // absent again once its reply is ingested) and Authored (an id and a handle).
    BoundSpec {
        name: HANDOFF_PENDING,
        source: Source::Session,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_handoff_id,
    },
    BoundSpec {
        name: HANDOFF_RECEIVED,
        source: Source::Session,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_draft_handle,
    },
    // items.id=897: the launch names. The five slots are Always (a Focus that has
    // the question always has a value: the answer, or the question's default, which
    // LOAD guarantees is one of its options) and Authored (the options are per-Focus
    // data, so the validator is shape only and the submit command checks membership
    // against the shipped data). run_change is Optional (absent means no change).
    BoundSpec {
        name: DOCUMENT_TYPE,
        source: Source::Launch,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_launch_answer,
    },
    BoundSpec {
        name: LAUNCH_MODE,
        source: Source::Launch,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_launch_answer,
    },
    BoundSpec {
        name: LAUNCH_ANSWER_1,
        source: Source::Launch,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_launch_answer,
    },
    BoundSpec {
        name: LAUNCH_ANSWER_2,
        source: Source::Launch,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_launch_answer,
    },
    BoundSpec {
        name: LAUNCH_ANSWER_3,
        source: Source::Launch,
        presence: Presence::Always,
        literals: Literals::Authored,
        validate: valid_launch_answer,
    },
    BoundSpec {
        name: RUN_CHANGE,
        source: Source::Launch,
        presence: Presence::Optional,
        literals: Literals::Authored,
        validate: valid_run_change,
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

/// A hand-off id: `h_` and exactly 12 lowercase hex characters.
fn valid_handoff_id(s: &str) -> bool {
    s.len() == 14
        && s.starts_with("h_")
        && s[2..]
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A draft handle, exactly as handles::draft_handle writes it.
fn valid_draft_handle(s: &str) -> bool {
    crate::persistence::handles::is_draft_handle(s)
}

/// A launch answer: a lower-case name of 1 to LAUNCH_ANSWER_MAX_CHARS characters
/// (letters, digits, underscores, starting with a letter). Shape only: whether it
/// is one of a question's options is the submit command's check against the shipped
/// data, and fails closed there.
fn valid_launch_answer(s: &str) -> bool {
    valid_identifier(s) && s.len() <= LAUNCH_ANSWER_MAX_CHARS
}

/// A run_change value: a JSON object that parses as a SettingsPatch (so it can name
/// only the six visible settings: an envelope key, a tier or a gate does not parse,
/// deny_unknown_fields) and fits the session store. Whether each named step exists
/// is checked against the scales when the settings are resolved.
fn valid_run_change(s: &str) -> bool {
    s.len() <= RUN_CHANGE_MAX_BYTES
        && !s.contains('\0')
        && s.trim_start().starts_with('{')
        && serde_json::from_str::<crate::conductor::model_block::document_type::SettingsPatch>(s)
            .is_ok()
}

/// An artifact value: at most pass_artifact_store::ARTIFACT_MAX_BYTES, no NUL,
/// not blank. (Seeding cleans braces before it validates, so the cleaned text
/// is what is checked.)
fn valid_artifact(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= crate::persistence::pass_artifact_store::ARTIFACT_MAX_BYTES
        && !s.contains('\0')
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
        for n in [
            OPEN_TICKETS_BRIEF,
            CASE_CONTEXT,
            DELTA_CONTEXT,
            SUBJECT_CONTEXT,
            DEVICE_LIBRARY_BRIEF,
        ] {
            assert_eq!(row(n), (Source::Derived, Presence::Always, None), "{n}");
        }
        for n in ARTIFACT_NAMES {
            assert_eq!(row(n), (Source::Artifact, Presence::Optional, None), "{n}");
        }
        for n in HANDOFF_NAMES {
            assert_eq!(row(n), (Source::Session, Presence::Optional, None), "{n}");
        }
        // items.id=897: five Always slots and one Optional run_change, all Launch,
        // all Authored (the options are per-Focus data, never a compiled Closed list).
        for n in LAUNCH_SLOT_NAMES {
            assert_eq!(row(n), (Source::Launch, Presence::Always, None), "{n}");
        }
        assert_eq!(row(RUN_CHANGE), (Source::Launch, Presence::Optional, None));
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

    /// items.id=895: the artifact names are exactly the closed kinds of the
    /// store, one name per kind and no other Artifact-source name.
    #[test]
    fn the_artifact_names_are_the_store_kinds() {
        use crate::persistence::pass_artifact_store::ArtifactKind;
        let kinds: Vec<&str> = ArtifactKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(kinds, ARTIFACT_NAMES);
        let from_specs: Vec<&str> = SPECS
            .iter()
            .filter(|s| s.source == Source::Artifact)
            .map(|s| s.name)
            .collect();
        assert_eq!(from_specs, ARTIFACT_NAMES);
        for n in ARTIFACT_NAMES {
            assert!(ArtifactKind::parse(n).is_some(), "{n}");
        }
    }

    /// items.id=387: the store module and the spec table name the same two
    /// variables, and the validators accept exactly what the writers produce.
    #[test]
    fn the_hand_off_names_match_the_store_and_validate_what_is_written() {
        use crate::persistence::handles::draft_handle;
        use crate::persistence::session_store::{HANDOFF_PENDING_NAME, HANDOFF_RECEIVED_NAME};
        assert_eq!(HANDOFF_PENDING, HANDOFF_PENDING_NAME);
        assert_eq!(HANDOFF_RECEIVED, HANDOFF_RECEIVED_NAME);
        let pending = spec(HANDOFF_PENDING).unwrap();
        for ok in ["h_0123456789ab", "h_ffffffffffff", "h_000000000000"] {
            assert!((pending.validate)(ok), "{ok}");
        }
        for bad in [
            "",
            "h_0123456789a",
            "h_0123456789abc",
            "H_0123456789ab",
            "h_0123456789AB",
            "h_0123456789ag",
            "x_0123456789ab",
            "h-0123456789ab",
            " h_0123456789ab",
        ] {
            assert!(!(pending.validate)(bad), "{bad}");
        }
        let received = spec(HANDOFF_RECEIVED).unwrap();
        assert!((received.validate)(&draft_handle(
            "writing-assistant",
            "out-1"
        )));
        for bad in [
            "",
            "H-abcdefgh",
            "T-ABCDEFGH",
            "H-ABCDEFG",
            "h_0123456789ab",
        ] {
            assert!(!(received.validate)(bad), "{bad}");
        }
    }

    #[test]
    fn the_artifact_validator_matches_the_store_limits() {
        use crate::persistence::pass_artifact_store::ARTIFACT_MAX_BYTES;
        assert!(valid_artifact("a brief"));
        assert!(valid_artifact(&"a".repeat(ARTIFACT_MAX_BYTES)));
        assert!(!valid_artifact(&"a".repeat(ARTIFACT_MAX_BYTES + 1)));
        assert!(!valid_artifact(""));
        assert!(!valid_artifact("  \n"));
        assert!(!valid_artifact("a\0b"));
    }

    /// items.id=897: the launch names are exactly the Launch-source rows, the five
    /// slots are exactly the slots a launch question may own, the resolver's profile
    /// name is registered here, and a Focus can never write one.
    #[test]
    fn the_launch_names_match_the_slots_the_resolver_and_the_data_use() {
        let from_specs: Vec<&str> = SPECS
            .iter()
            .filter(|s| s.source == Source::Launch)
            .map(|s| s.name)
            .collect();
        assert_eq!(from_specs, LAUNCH_NAMES);
        assert_eq!(LAUNCH_SLOT_NAMES, crate::conductor::launch_config::SLOTS);
        assert_eq!(
            &LAUNCH_NAMES[..5],
            &LAUNCH_SLOT_NAMES[..],
            "run_change is the one launch name that is not a question slot"
        );
        // The rider of items.id=896 stage 3: the resolver reads its profile from the
        // seed of this name, and the name is registered.
        assert_eq!(
            crate::conductor::model_resolver_input::DOCUMENT_TYPE_NAME,
            DOCUMENT_TYPE
        );
        assert!(spec(DOCUMENT_TYPE).is_some());
        // The slots the SQL CHECK admits are the same five.
        assert_eq!(
            crate::conductor::launch_config::SLOT_DOCUMENT_TYPE,
            DOCUMENT_TYPE
        );
        assert_eq!(
            crate::conductor::launch_config::SLOT_LAUNCH_MODE,
            LAUNCH_MODE
        );
    }

    #[test]
    fn the_launch_validators_check_shape_and_a_run_change_parses_as_a_settings_patch() {
        let slot = spec(LAUNCH_MODE).unwrap();
        for ok in ["quick", "document_polished", "a", "rma_letter2"] {
            assert!((slot.validate)(ok), "{ok}");
        }
        for bad in [
            "",
            "Quick",
            "2fast",
            "has space",
            "{brace}",
            "a-b",
            &"a".repeat(LAUNCH_ANSWER_MAX_CHARS + 1),
        ] {
            assert!(!(slot.validate)(bad), "{bad}");
        }
        assert!((slot.validate)(&"a".repeat(LAUNCH_ANSWER_MAX_CHARS)));

        let change = spec(RUN_CHANGE).unwrap();
        for ok in [
            "{}",
            r#"{"tone":"warm"}"#,
            r#"{"tone":"firm","length":"short","latitude":"strict"}"#,
        ] {
            assert!((change.validate)(ok), "{ok}");
        }
        // An envelope key, a gate, an unknown key, a non-object or an oversized value
        // is refused: a run_change moves the visible settings and nothing else.
        for bad in [
            "",
            "null",
            "[]",
            "\"tone\"",
            r#"{"highest_tier":"unrestricted"}"#,
            r#"{"destinations":["hosted_api"]}"#,
            r#"{"quality_floor":{"min":"basic","max":"high"}}"#,
            r#"{"declared_inputs":["user_input"]}"#,
            r#"{"gate1":false}"#,
            r#"{"tone":5}"#,
        ] {
            assert!(!(change.validate)(bad), "{bad}");
        }
        let huge = format!(r#"{{"tone":"{}"}}"#, "a".repeat(RUN_CHANGE_MAX_BYTES));
        assert!(!(change.validate)(&huge));
    }
}

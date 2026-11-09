// src-tauri/src/conductor/tech_support_draft.rs
//
// items.id=889: the SIX-INTENT TECH SUPPORT DRAFT, a TEST FIXTURE ONLY.
//
// It exists to prove that the guard language (items.id=878), the typed store
// writers (items.id=887) and the AND-guards, handles and operations of
// items.id=889 can express the shape decisions.id=940 plans for Tech Support
// (new_issue, continue_ticket, check_in_update, close_ticket, device_library,
// one_time_session, plus the disambiguation of decisions.id=515 Cases A, B and
// C), and that it loads through the real loader (lifecycle tests) with a path
// count under the cap. It is NOT a Focus: nothing seeds it, nothing ships it, no
// shipped Focus is wired to it, and its prompts are one-line placeholders, not
// authored content (items.id=129 Stage 1b owns the real Focus).
//
// THE SHAPE (one chat message is one pass):
//
//   classify           one local call, every pass: the message to one of seven
//                      intents (the six, plus `unsure`), shown the locked
//                      intent and the ticket state.
//   latch / lock       one_time_session sets the latch, every other intent is
//                      remembered in locked_intent (only while the chat saves).
//   disambiguation     Case A: unsure with ONE open ticket, Case B: unsure with
//                      MANY, Case C: unsure with NONE proceeds straight into the
//                      new-issue flow (an `in` clause makes `unsure` and
//                      `new_issue` one condition). A new_issue message while
//                      tickets are open surfaces them first (the new-instance
//                      confirmation of decisions.id=515).
//   new issue          extract entity, label, affected entity and symptoms (four
//                      local calls, the three that only feed writers run only
//                      while the chat saves), open the case, make it the active
//                      case, link the affected entity, reply.
//   continue ticket    the model names a HANDLE from open_tickets_brief,
//                      case_select resolves it store-side, reply.
//   check in update    the model says whether an event happened on the device
//                      (entity_changed moves its deltas to believed-changed) or
//                      names a delta HANDLE the user confirmed (delta_confirm).
//   close ticket       a closure state, a closing note, the strategy log, THEN
//                      the closing case_update (the close-ticket lint requires
//                      it to be the last case write), reply, then session_clear.
//   device library     reply from the reference material.
//   one time session   latch, reply.
//   fallback           any message no row answered (intent OTHER, or a ticket
//                      that cannot be found) gets one clarifying reply.
//
// `reply` is the output variable of every user-facing row. The run's output is
// the LAST model step (task_track.last_output), so on every path the reply is
// the last model row, and a writer after it records nothing.
//
// Two variants: Lean guards the extraction rows that only feed writers on
// write_mode (a one-time chat makes no call whose result nothing reads), Naive
// does not. The acceptance test reports the wasted local calls of both, which is
// what makes the number mean something.

/// (variable, operator, value): the typed clause, as the loader reads it.
pub type Clause = (&'static str, &'static str, Option<&'static str>);

#[derive(Clone)]
pub enum Kind {
    /// A local model step: its template and the variable it records.
    Model {
        template: &'static str,
        out: &'static str,
    },
    /// A store writer: operation, optional target, optional args JSON.
    Writer {
        op: &'static str,
        target: Option<&'static str>,
        args: Option<&'static str>,
    },
}

#[derive(Clone)]
pub struct DraftRow {
    pub id: &'static str,
    pub kind: Kind,
    /// Every clause must hold. The first goes in the single guard columns, the
    /// rest in guard_clauses, so both columns are exercised.
    pub clauses: Vec<Clause>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Extraction rows that only feed writers run only while write_mode is open.
    Lean,
    /// The same rows without that clause.
    Naive,
}

const INTENTS_THAT_LOCK: &str =
    r#"["new_issue","continue_ticket","check_in_update","close_ticket","device_library"]"#;
const NEW_ISSUE_OR_UNSURE: &str = r#"["new_issue","unsure"]"#;
const TERMINAL: &str = r#"["resolved","replaced","closed"]"#;

fn m(
    id: &'static str,
    out: &'static str,
    template: &'static str,
    clauses: Vec<Clause>,
) -> DraftRow {
    DraftRow {
        id,
        kind: Kind::Model { template, out },
        clauses,
    }
}

fn w(
    id: &'static str,
    op: &'static str,
    target: Option<&'static str>,
    args: Option<&'static str>,
    clauses: Vec<Clause>,
) -> DraftRow {
    DraftRow {
        id,
        kind: Kind::Writer { op, target, args },
        clauses,
    }
}

/// The draft's rows, in sequence order.
pub fn rows(variant: Variant) -> Vec<DraftRow> {
    // Case C and the plain new issue with no open ticket: one condition.
    let nc: Clause = ("intent", "in", Some(NEW_ISSUE_OR_UNSURE));
    let none: Clause = ("open_ticket_state", "eq", Some("none"));
    let saves: Clause = ("write_mode", "eq", Some("open"));
    let ac: Clause = ("active_case_id", "present", None);
    let terminal: Clause = ("closure", "in", Some(TERMINAL));
    // A row that only feeds writers is guarded on write_mode in Lean.
    let feeds_writers = |mut c: Vec<Clause>| {
        if variant == Variant::Lean {
            c.push(saves);
        }
        c
    };

    vec![
        // -- every pass ------------------------------------------------------
        m(
            "classify",
            "intent",
            "Name the intent of the message: new_issue, continue_ticket, check_in_update, \
             close_ticket, device_library, one_time_session or unsure. Locked intent: \
             {locked_intent}. Open tickets: {open_ticket_state}. Message: {user_input}",
            vec![],
        ),
        w(
            "one_time",
            "latch",
            None,
            None,
            vec![("intent", "eq", Some("one_time_session"))],
        ),
        w(
            "lock_intent",
            "session_put",
            Some("locked_intent"),
            Some(r#"{"value":"intent"}"#),
            vec![("intent", "in", Some(INTENTS_THAT_LOCK)), saves],
        ),
        // -- disambiguation, decisions.id=515 --------------------------------
        // Case A: one open ticket and the intent is not clear.
        m(
            "ask_one_ticket",
            "reply",
            "One open ticket: {open_tickets_brief}. Ask whether this is that ticket or \
             something new. Message: {user_input}",
            vec![
                ("intent", "eq", Some("unsure")),
                ("open_ticket_state", "eq", Some("one")),
            ],
        ),
        // Case B: several open tickets and the intent is not clear.
        m(
            "ask_which_ticket",
            "reply",
            "Open tickets: {open_tickets_brief}. Ask which one this is, or whether it is \
             something new. Message: {user_input}",
            vec![
                ("intent", "eq", Some("unsure")),
                ("open_ticket_state", "eq", Some("many")),
            ],
        ),
        // A new issue while tickets are open: surface them first.
        m(
            "confirm_new",
            "reply",
            "Open tickets: {open_tickets_brief}. Ask whether the new message belongs to one \
             of them or is separate. Message: {user_input}",
            vec![
                ("intent", "eq", Some("new_issue")),
                ("open_ticket_state", "ne", Some("none")),
            ],
        ),
        // -- new issue, and Case C: no open ticket, so straight in -----------
        m(
            "nc_entity",
            "entity_ref",
            "Name the device. Message: {user_input}",
            feeds_writers(vec![nc, none]),
        ),
        m(
            "nc_label",
            "label",
            "Give the device a short label. Message: {user_input}",
            feeds_writers(vec![nc, none]),
        ),
        m(
            "nc_affected",
            "affected_ref",
            "Name a second affected device, if any. Message: {user_input}",
            feeds_writers(vec![nc, none]),
        ),
        m(
            "nc_symptoms",
            "symptoms",
            "Summarise the symptoms. Message: {user_input}",
            vec![nc, none],
        ),
        w(
            "open_case",
            "case_open",
            Some("new_case"),
            Some(r#"{"entity":"entity_ref","label":"label","symptoms":"symptoms"}"#),
            vec![nc, none, saves],
        ),
        w(
            "select_new",
            "session_put",
            Some("active_case_id"),
            Some(r#"{"value":"new_case"}"#),
            vec![("new_case", "present", None)],
        ),
        w(
            "link_affected",
            "case_link",
            None,
            Some(r#"{"case":"new_case","entity":"affected_ref"}"#),
            vec![
                ("new_case", "present", None),
                ("affected_ref", "present", None),
            ],
        ),
        m(
            "nc_reply",
            "reply",
            "Begin the interview about: {symptoms}",
            vec![nc, none],
        ),
        // -- continue ticket -------------------------------------------------
        m(
            "pick_ticket",
            "ticket_handle",
            "From the open tickets, say only the handle of the one meant: \
             {open_tickets_brief}. Message: {user_input}",
            vec![
                ("intent", "eq", Some("continue_ticket")),
                ("open_ticket_state", "ne", Some("none")),
            ],
        ),
        w(
            "select_ticket",
            "case_select",
            None,
            Some(r#"{"handle":"ticket_handle"}"#),
            vec![("ticket_handle", "present", None), saves],
        ),
        m(
            "resume_reply",
            "reply",
            "Resume the ticket: {open_tickets_brief}. Message: {user_input}",
            vec![
                ("intent", "eq", Some("continue_ticket")),
                ("ticket_handle", "present", None),
            ],
        ),
        // -- check in update -------------------------------------------------
        m(
            "ci_event",
            "event",
            "Did the user report a change on the device (changed or unchanged)? \
             Message: {user_input}",
            vec![("intent", "eq", Some("check_in_update")), ac],
        ),
        w(
            "ci_changed",
            "entity_changed",
            None,
            Some(r#"{"case":"active_case_id"}"#),
            vec![
                ("intent", "eq", Some("check_in_update")),
                ("event", "eq", Some("changed")),
                ac,
                saves,
            ],
        ),
        m(
            "ci_pick",
            "delta_handle",
            "From the earlier changes, say only the handle of the one the user confirmed: \
             {delta_context}. Message: {user_input}",
            vec![
                ("intent", "eq", Some("check_in_update")),
                ("event", "eq", Some("unchanged")),
                ac,
                saves,
            ],
        ),
        w(
            "ci_confirm",
            "delta_confirm",
            None,
            Some(r#"{"handle":"delta_handle"}"#),
            vec![("delta_handle", "present", None), saves],
        ),
        m(
            "ci_reply",
            "reply",
            "Respond to the check-in. Message: {user_input}",
            vec![
                ("intent", "eq", Some("check_in_update")),
                ("event", "present", None),
            ],
        ),
        // -- close ticket ----------------------------------------------------
        m(
            "closure",
            "closure",
            "Say resolved, replaced, closed or open. Message: {user_input}",
            vec![("intent", "eq", Some("close_ticket")), ac],
        ),
        m(
            "close_note",
            "closing_note",
            "Write a one-line closing note for the log. Message: {user_input}",
            feeds_writers(vec![("intent", "eq", Some("close_ticket")), terminal]),
        ),
        w(
            "close_log",
            "strategy_append",
            None,
            Some(r#"{"case":"active_case_id","strategy":"closing_note","outcome":"closing_note"}"#),
            vec![terminal, ac, saves],
        ),
        // The closing write: the LAST case write on its path (the lint).
        w(
            "close_write",
            "case_update",
            None,
            Some(r#"{"case":"active_case_id","state":"closure","outcome":"closing_note"}"#),
            vec![("intent", "eq", Some("close_ticket")), terminal, ac, saves],
        ),
        m(
            "close_reply",
            "reply",
            "Confirm the ticket is closed. Message: {user_input}",
            vec![
                ("intent", "eq", Some("close_ticket")),
                ("closure", "present", None),
            ],
        ),
        w(
            "close_clear",
            "session_clear",
            None,
            None,
            vec![terminal, saves],
        ),
        // -- device library, one time session --------------------------------
        m(
            "dl_reply",
            "reply",
            "Answer from the device reference: {case_context}. Message: {user_input}",
            vec![("intent", "eq", Some("device_library"))],
        ),
        m(
            "ot_reply",
            "reply",
            "Help with this one-time question, nothing is saved. Message: {user_input}",
            vec![("intent", "eq", Some("one_time_session"))],
        ),
        // -- fallback: nothing above answered --------------------------------
        m(
            "fallback_reply",
            "reply",
            "Ask what the user needs. Message: {user_input}",
            vec![("intent", "present", None), ("reply", "absent", None)],
        ),
    ]
}

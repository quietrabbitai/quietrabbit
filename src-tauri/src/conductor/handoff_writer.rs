// src-tauri/src/conductor/handoff_writer.rs
//
// items.id=387 (decisions.id=942 point 7, S2): the handler of the typed store
// operation handoff_ingest, and the three types that carry the one thing no other
// writer ever touches: the RAW TEXT THE USER TYPED OR PASTED IN THIS TURN.
//
// WHAT THE OPERATION DOES. A manual hand-off sends the user away with a starter
// (the outbound pass, S3) and takes their pasted reply back as the NEXT chat turn.
// handoff_ingest runs in that return pass, guarded on handoff_pending (LOAD
// refuses it unguarded). It stores the pasted turn as an untrusted draft through
// the Library's ingestion (persistence::handoff_draft_store), then in ONE
// transaction clears handoff_pending and sets handoff_received to the draft's H-
// handle (session_store::settle_handoff_return), and PUBLISHES that handle under
// the row's required store_target for later rows of the pass. Nothing is recorded
// on the TaskTrack, so nothing it stores reaches extraction or {previous_output}.
//
// THE ONE EXCEPTION (recorded in the header of store_op.rs). Every other writer
// takes its values from NAMED VARIABLES, and LOAD refuses a system token such as
// {user_input} as an argument: raw user text is never a store argument. A paste
// cannot follow that rule. A variable would be a model's copy of it, not the
// user's text, and the whole point is the exact text, untouched. So handoff_ingest
// has NO slots and reads the turn from here. The compiler and a source scan hold it
// to that:
//   * the turn lives in CurrentTurn, owned by the run, handed to the writer as a
//     TurnText that only THIS module can read (the accessor is private), so no
//     other operation can read it;
//   * CurrentTurn and TurnText are not Clone (a TurnText is Copy, but only a
//     borrow), not Serialize, not Display, and their Debug redacts the text. A
//     checkpoint, a snapshot, a focus_runs row and a log line cannot be made from
//     them;
//   * the run never persists it and rehydrate_focus_run never sets it, so a
//     resumed run has none and the operation fails closed (HandoffTurnUnavailable);
//   * the one log line of the operation carries a byte count and nothing else.
// The blended chat window ({user_input}, focus_runs.user_input) is a separate,
// older path that already carries the paste as the newest message: that is not
// this type, and the taint rule of taint.rs keeps the steps that name it out of
// extraction in a Focus that has a handoff_ingest row.
//
// ORDER. The one-time latch is read BEFORE anything is stored, so a latched chat
// writes no blob. Consent is checked before that, by run_store_op, for every
// operation that saves user content. Then: a turn must exist, the paste must
// satisfy the caps (persistence::handoff_draft_store::validate_paste), a hand-off
// must be pending. A crash between the draft and the session transaction leaves
// a stored reply the chat does not point at and handoff_pending still set: the
// retry stores a second reply. That window is accepted: both are the user's own
// Library documents, and nothing is lost.

use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{self, HANDOFF_PENDING};
use crate::conductor::store_op::StoreOp;
use crate::conductor::store_writer::{
    done, refused, Done, Outcome, StoreWriteError, WriterContext,
};
use crate::persistence::handles::draft_handle;
use crate::persistence::handoff_draft_store::{self, validate_paste};
use crate::persistence::session_store::{self, WriteMode};

/// The raw text of the turn a pass is answering, owned by the run. Built by the
/// sender of the turn (commands::messages) and never by anything else. Not Clone,
/// not Serialize, not Display; Debug redacts it.
pub struct CurrentTurn(String);

impl CurrentTurn {
    pub fn new(text: String) -> Self {
        CurrentTurn(text)
    }

    /// A borrow for one writer context. The text is still unreadable outside this
    /// module.
    pub(crate) fn lend(&self) -> TurnText<'_> {
        TurnText(&self.0)
    }
}

impl std::fmt::Debug for CurrentTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CurrentTurn(<redacted, {} bytes>)", self.0.len())
    }
}

/// A borrowed CurrentTurn inside a WriterContext. Only handoff_writer reads it.
#[derive(Clone, Copy)]
pub struct TurnText<'a>(&'a str);

impl TurnText<'_> {
    /// Private on purpose: no other module can read the turn.
    fn text(&self) -> &str {
        self.0
    }
}

impl std::fmt::Debug for TurnText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TurnText(<redacted, {} bytes>)", self.0.len())
    }
}

/// The session's data key in hex, for the one writer that stores a document.
/// Debug redacts it so WriterContext's Debug cannot print a key.
#[derive(Clone, Copy)]
pub struct SecretHex<'a>(&'a str);

impl<'a> SecretHex<'a> {
    pub fn new(hex: &'a str) -> Self {
        SecretHex(hex)
    }

    fn expose(&self) -> &str {
        self.0
    }
}

impl std::fmt::Debug for SecretHex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretHex(<redacted>)")
    }
}

/// Run one handoff_ingest row. `key` is the chat's context key (run_store_op has
/// already required it and the collection consent).
pub(crate) async fn run_handoff_ingest(
    conn: &mut SqliteConnection,
    key: &str,
    ctx: &WriterContext<'_>,
    op: &StoreOp,
) -> Result<Done, StoreWriteError> {
    let turn = ctx
        .turn_text
        .ok_or(StoreWriteError::HandoffTurnUnavailable)?;
    let text = turn.text();
    let data_key = match ctx.data_key {
        Some(k) if !ctx.user_id.is_empty() => k,
        _ => return Err(StoreWriteError::HandoffNoStoreAccess),
    };

    // The latch first: a one-time chat stores nothing, not even a blob.
    let mode = session_store::write_mode(conn, key)
        .await
        .map_err(session_store::SessionStoreError::from)?;
    if mode == WriteMode::Suppressed {
        return Ok(refused());
    }

    // A hand-off must be waiting for this reply.
    let pending =
        session_store::get_session_variable(conn, key, ctx.focus_id, HANDOFF_PENDING).await?;
    match pending {
        None => return Err(session_store::SessionStoreError::HandoffNotPending.into()),
        Some(v) => {
            let valid = bound_spec::spec(HANDOFF_PENDING).is_some_and(|s| (s.validate)(&v.value));
            if !valid {
                return Err(StoreWriteError::InvalidSessionValue(
                    HANDOFF_PENDING.to_owned(),
                ));
            }
        }
    }

    validate_paste(text)?;
    let output_id = handoff_draft_store::store_draft(
        ctx.user_id,
        ctx.persona_id,
        data_key.expose(),
        ctx.focus_id,
        text,
    )
    .await?;
    let handle = draft_handle(ctx.focus_id, &output_id);
    session_store::settle_handoff_return(conn, key, ctx.focus_id, &handle, ctx.run_id).await?;

    log_handoff_write(Outcome::Applied, text.len());
    let mut d = done(Outcome::Applied);
    d.published = op.published_var().map(|v| (v.to_owned(), handle));
    Ok(d)
}

/// THE ONE LOG CALL of this operation: the outcome and a byte count. It takes no
/// text and no hash, so none can reach a log.
fn log_handoff_write(outcome: Outcome, bytes: usize) {
    log::debug!("store writer: handoff_ingest -> {outcome:?} ({bytes} bytes)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_carries_the_turn_or_the_key() {
        let secret = "the-secret-pasted-reply";
        let turn = CurrentTurn::new(secret.to_owned());
        assert!(!format!("{turn:?}").contains(secret));
        assert!(format!("{turn:?}").contains("redacted"));
        let lent = turn.lend();
        assert!(!format!("{lent:?}").contains(secret));
        let key = SecretHex::new("00112233445566778899aabbccddeeff");
        assert!(!format!("{key:?}").contains("0011"));
        let opt = Some(lent);
        assert!(!format!("{opt:?}").contains(secret));
    }

    /// The type carries nothing that could serialize, clone or print the text.
    /// Source-level pin: the declarations are scanned, because a missing trait is
    /// not something a test can call.
    #[test]
    fn the_turn_types_derive_nothing_that_could_copy_or_print_the_text() {
        let src = include_str!("handoff_writer.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for ty in ["CurrentTurn", "TurnText", "SecretHex"] {
            let decl_at = body
                .find(&format!("pub struct {ty}"))
                .unwrap_or_else(|| panic!("{ty} not declared"));
            // The attribute lines directly above the declaration.
            let above: Vec<&str> = body[..decl_at]
                .lines()
                .rev()
                .take_while(|l| l.trim_start().starts_with("#[") || l.trim().is_empty())
                .collect();
            let attrs = above.join(" ");
            for banned in ["Serialize", "Deserialize", "Debug", "PartialEq", "Hash"] {
                assert!(!attrs.contains(banned), "{ty} must not derive {banned}");
            }
            // Only the two borrows may be Copy, and CurrentTurn is not Clone.
            if ty == "CurrentTurn" {
                assert!(!attrs.contains("Clone") && !attrs.contains("Copy"), "{ty}");
            }
        }
        assert!(!body.contains("impl std::fmt::Display"));
        assert!(!body.contains("impl Clone for CurrentTurn"));
        assert!(!body.contains("impl Serialize"));
    }

    #[test]
    fn this_module_has_exactly_one_log_call_and_it_carries_no_text() {
        let src = include_str!("handoff_writer.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        assert_eq!(body.matches("log::").count(), 1);
        let call = body
            .lines()
            .find(|l| l.contains("log::debug!"))
            .expect("the log call");
        assert!(call.contains("bytes"));
        for banned in ["text", "turn", "handle", "paste", "key"] {
            assert!(!call.contains(banned), "the log call names {banned}");
        }
        for macro_name in [
            "println!",
            "eprintln!",
            "dbg!",
            "trace!(",
            "info!(",
            "warn!(",
        ] {
            assert!(!body.contains(macro_name), "{macro_name}");
        }
    }
}

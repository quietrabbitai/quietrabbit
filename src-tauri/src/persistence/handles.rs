// src-tauri/src/persistence/handles.rs
//
// items.id=889 (decisions.id=940 ruling 1, decisions.id=938 G2): OPAQUE HANDLES
// for tickets and configuration changes.
//
// A model can never be shown an internal id: case ids are "{run_id}:{action_id}"
// and delta ids the same, and a model that could name one could name another
// Focus's. Derived text (open_tickets_brief, case_context, delta_context) shows
// a short HANDLE instead, and a typed store writer (case_select, delta_confirm)
// takes the handle back. Resolution is HERE, in the store layer, scoped by the
// running focus_id, so a handle that is wrong, foreign, stale or made up writes
// nothing and the writer fails closed.
//
// SHAPE. T-XXXXXXXX for a ticket, D-XXXXXXXX for a configuration change: a kind
// letter, a hyphen, and eight characters of Crockford base32 (digits and capital
// letters without I, L, O and U), which is 40 bits.
//
// STABLE WITHOUT A COLUMN. A handle is a pure function of (kind, focus_id, id):
// the first five bytes of SHA-256 over a versioned, NUL-separated string. It is
// the same on every pass and on every device, needs no schema, and cannot be
// inverted to the id. The kind and the Focus are inside the hash, so a ticket
// handle never resolves as a delta handle and the same case id under another
// Focus has a different handle. Because it is derived, resolution LISTS the
// Focus's own candidates and compares: a handle matches only a row that
// belongs to the running Focus, and two rows of one Focus that collide (about
// 2^-40 per pair) make the handle ambiguous, which refuses rather than picks.
//
// WHAT A HANDLE REACHES:
//   ticket  an OPEN case (any non-terminal state) that this Focus opened. A
//           closed case is not selectable: it takes no writes (case_store).
//   delta   any configuration change this Focus recorded (config_deltas.focus_id).
//
// ENTITY HANDLES (items.id=891). E-XXXXXXXX names a device the running Focus OWNS
// (entities.owner_focus, personal_011.sql), from the same derivation. It is the
// one way a model picks an EXISTING device: device_library_brief shows handles, a
// model step names one, and entity_find_or_create resolves it store-side among the
// Focus's own active entities in personal.db. Another Focus's device, an
// unowned entity, an archived or purge-marked one and an invented handle all
// resolve to nothing. An entity id is never shown to a model and never accepted
// from one.
//
// DRAFT HANDLES (items.id=387). H-XXXXXXXX names a pasted hand-off reply the
// running Focus stored (persistence::handoff_draft_store): an ingested output of
// type handoff_draft whose focus_slug is the Focus and that has not been deleted.
// The text is untrusted external content and is read only through the store
// reader, by handle, never shown to a template through this module.
//
// A handle is not a secret and not an authorization: it is an unguessable name
// for a row the Focus is already allowed to touch. Error text never echoes a
// value (the handle is model output).
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros. READS
// ONLY: nothing here writes.

use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use crate::persistence::case_store::{self, CaseStoreError};
use crate::persistence::owned_entity_store::{self, OwnedEntityError};

/// Versioned so a future change of the derivation is a deliberate break.
const DOMAIN: &str = "qr-handle-v1";
/// Crockford base32: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters after the kind letter and hyphen.
pub const HANDLE_CHARS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ticket,
    Delta,
    Entity,
    Draft,
}

impl Kind {
    fn letter(self) -> char {
        match self {
            Kind::Ticket => 'T',
            Kind::Delta => 'D',
            Kind::Entity => 'E',
            Kind::Draft => 'H',
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Kind::Ticket => "case",
            Kind::Delta => "delta",
            Kind::Entity => "entity",
            Kind::Draft => "draft",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HandleError {
    #[error("the handle is not in the form {0}-XXXXXXXX")]
    Malformed(char),
    #[error("no such {0} for this Focus")]
    NotFound(&'static str),
    #[error("the handle matches more than one {0} of this Focus")]
    Ambiguous(&'static str),
    #[error("{0}")]
    Store(#[from] CaseStoreError),
    #[error("{0}")]
    Owned(#[from] OwnedEntityError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The handle of one row. A pure function: same inputs, same text.
pub fn handle_for(kind: Kind, focus_id: &str, id: &str) -> String {
    let mut h = Sha256::new();
    for part in [DOMAIN, kind.tag(), focus_id, id] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let digest = h.finalize();
    let mut bits: u64 = 0;
    for b in &digest[..5] {
        bits = (bits << 8) | u64::from(*b);
    }
    let mut out = String::with_capacity(2 + HANDLE_CHARS);
    out.push(kind.letter());
    out.push('-');
    for i in (0..HANDLE_CHARS).rev() {
        out.push(ALPHABET[((bits >> (5 * i)) & 31) as usize] as char);
    }
    out
}

pub fn case_handle(focus_id: &str, case_id: &str) -> String {
    handle_for(Kind::Ticket, focus_id, case_id)
}

pub fn delta_handle(focus_id: &str, delta_id: &str) -> String {
    handle_for(Kind::Delta, focus_id, delta_id)
}

/// items.id=891: the handle of a device a Focus owns.
pub fn entity_handle(focus_id: &str, entity_id: &str) -> String {
    handle_for(Kind::Entity, focus_id, entity_id)
}

/// items.id=387: the handle of a pasted hand-off reply a Focus stored.
pub fn draft_handle(focus_id: &str, output_id: &str) -> String {
    handle_for(Kind::Draft, focus_id, output_id)
}

/// True when `raw` is shaped like an entity handle (E-XXXXXXXX, however a model
/// echoes it). Text that is NOT handle-shaped (a model saying "none") is not a
/// handle at all and the caller goes on without one. Text that IS handle-shaped
/// and resolves to nothing is an error: that is an invented or stale handle.
pub fn looks_like_entity_handle(raw: &str) -> bool {
    canonical(raw, Kind::Entity).is_some()
}

/// Reads a model-supplied handle into its canonical text, or None when it is not
/// one. Whitespace around it and one surrounding pair of square brackets are
/// allowed, and case is folded: those are the ways a model echoes a token it was
/// shown. Anything else (another kind, a wrong length, a character outside the
/// alphabet, trailing text) is not a handle.
fn canonical(raw: &str, kind: Kind) -> Option<String> {
    let t = raw.trim();
    let t = t
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(t)
        .trim();
    let upper = t.to_ascii_uppercase();
    let body = upper.strip_prefix(kind.letter())?.strip_prefix('-')?;
    let ok = body.len() == HANDLE_CHARS && body.bytes().all(|b| ALPHABET.contains(&b));
    ok.then_some(upper)
}

fn pick(matches: Vec<String>, what: &'static str) -> Result<String, HandleError> {
    let mut it = matches.into_iter();
    match (it.next(), it.next()) {
        (None, _) => Err(HandleError::NotFound(what)),
        (Some(id), None) => Ok(id),
        (Some(_), Some(_)) => Err(HandleError::Ambiguous(what)),
    }
}

/// The case id behind a ticket handle: an open case that `focus_id` opened.
pub async fn resolve_case_handle(
    conn: &mut SqliteConnection,
    focus_id: &str,
    handle: &str,
) -> Result<String, HandleError> {
    let want = canonical(handle, Kind::Ticket).ok_or(HandleError::Malformed('T'))?;
    let open = case_store::list_open_cases(conn, focus_id).await?;
    let matches: Vec<String> = open
        .into_iter()
        .filter(|c| case_handle(focus_id, &c.case_id) == want)
        .map(|c| c.case_id)
        .collect();
    pick(matches, "ticket")
}

/// The delta id behind a configuration-change handle: a delta `focus_id`
/// recorded.
pub async fn resolve_delta_handle(
    conn: &mut SqliteConnection,
    focus_id: &str,
    handle: &str,
) -> Result<String, HandleError> {
    let want = canonical(handle, Kind::Delta).ok_or(HandleError::Malformed('D'))?;
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT delta_id FROM config_deltas WHERE focus_id = ?")
            .bind(focus_id)
            .fetch_all(&mut *conn)
            .await?;
    let matches: Vec<String> = ids
        .into_iter()
        .filter(|id| delta_handle(focus_id, id) == want)
        .collect();
    pick(matches, "configuration change")
}

/// items.id=891: the entity id behind a device handle: an ACTIVE entity that
/// `focus_id` owns, read from the persona's personal.db (the connection is
/// personal.db, not the outputs.db the other two resolvers take).
pub async fn resolve_entity_handle(
    personal: &mut SqliteConnection,
    focus_id: &str,
    handle: &str,
) -> Result<String, HandleError> {
    let want = canonical(handle, Kind::Entity).ok_or(HandleError::Malformed('E'))?;
    let owned = owned_entity_store::list_owned_active(personal, focus_id).await?;
    let matches: Vec<String> = owned
        .into_iter()
        .filter(|e| entity_handle(focus_id, &e.id) == want)
        .map(|e| e.id)
        .collect();
    pick(matches, "device")
}

/// items.id=387: the output id behind a draft handle: a hand-off reply that
/// `focus_id` stored and that is not deleted, read from the persona's outputs.db.
pub async fn resolve_draft_handle(
    conn: &mut SqliteConnection,
    focus_id: &str,
    handle: &str,
) -> Result<String, HandleError> {
    let want = canonical(handle, Kind::Draft).ok_or(HandleError::Malformed('H'))?;
    let ids = crate::persistence::handoff_draft_store::list_draft_ids(conn, focus_id).await?;
    let matches: Vec<String> = ids
        .into_iter()
        .filter(|id| draft_handle(focus_id, id) == want)
        .collect();
    pick(matches, "hand-off reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_is_stable_short_and_in_the_documented_shape() {
        let h = case_handle("tech-support", "run-1:open");
        assert_eq!(h, case_handle("tech-support", "run-1:open"));
        assert_eq!(h.len(), 2 + HANDLE_CHARS);
        assert!(h.starts_with("T-"));
        assert!(h[2..].bytes().all(|b| ALPHABET.contains(&b)));
        assert!(delta_handle("tech-support", "run-1:rec").starts_with("D-"));
    }

    #[test]
    fn a_handle_never_contains_the_internal_id() {
        let h = case_handle("tech-support", "run-1:open");
        assert!(!h.contains("run-1") && !h.contains("open"));
    }

    #[test]
    fn kind_and_focus_are_inside_the_hash() {
        let id = "run-1:x";
        assert_ne!(case_handle("a", id)[2..], delta_handle("a", id)[2..]);
        assert_ne!(case_handle("a", id), case_handle("b", id));
    }

    #[test]
    fn distinct_ids_get_distinct_handles_over_a_large_sample() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..20_000 {
            assert!(
                seen.insert(case_handle("tech-support", &format!("run-{i}:open"))),
                "collision at {i}"
            );
        }
    }

    #[test]
    fn two_matches_are_ambiguous_and_never_picked() {
        assert!(matches!(
            pick(vec!["a".into(), "b".into()], "ticket"),
            Err(HandleError::Ambiguous("ticket"))
        ));
        assert!(matches!(
            pick(Vec::new(), "ticket"),
            Err(HandleError::NotFound("ticket"))
        ));
        assert_eq!(pick(vec!["a".into()], "ticket").unwrap(), "a");
    }

    #[test]
    fn error_text_never_carries_a_value() {
        for e in [
            HandleError::Malformed('T'),
            HandleError::NotFound("ticket"),
            HandleError::Ambiguous("ticket"),
        ] {
            let m = e.to_string();
            assert!(!m.contains("T-0000"), "{m}");
        }
    }

    #[test]
    fn canonical_accepts_the_ways_a_model_echoes_a_token_and_nothing_else() {
        let h = case_handle("f", "c");
        let lower = h.to_ascii_lowercase();
        for ok in [
            h.clone(),
            format!("  {h}\n"),
            lower,
            format!("[{h}]"),
            format!("[ {h} ]"),
        ] {
            assert_eq!(
                canonical(&ok, Kind::Ticket).as_deref(),
                Some(h.as_str()),
                "{ok}"
            );
        }
        for bad in [
            String::new(),
            "T-".to_owned(),
            format!("{h}x"),
            format!("{h}."),
            format!("Ticket {h}"),
            h.replacen("T-", "D-", 1),
            h.replacen("T-", "T", 1),
            format!("(({h}))"),
            "T-ILOU0000".to_owned(),
            "T-1234567".to_owned(),
            "T-123456789".to_owned(),
        ] {
            assert_eq!(canonical(&bad, Kind::Ticket), None, "{bad}");
        }
        // A ticket handle is not a delta handle.
        assert_eq!(canonical(&h, Kind::Delta), None);
    }

    // -- resolution against a real store (in-memory outputs.db) ---------------

    use crate::persistence::case_store::{
        open_case, record_config_delta, update_case, CaseState, CaseUpdate, NewCase, NewConfigDelta,
    };
    use crate::persistence::session_store::test_db::mem_outputs_conn;

    const FOCUS: &str = "tech-support";

    async fn add_case(conn: &mut SqliteConnection, focus: &str, id: &str) {
        open_case(
            conn,
            &NewCase {
                case_id: id,
                focus_id: focus,
                context_key: "chat-1",
                primary_entity_id: "ent-router",
                subject_label: "Router",
                symptoms: "",
                opened_by_run: "run-1",
            },
        )
        .await
        .unwrap();
    }

    async fn add_delta(conn: &mut SqliteConnection, focus: &str, id: &str) {
        record_config_delta(
            conn,
            &NewConfigDelta {
                delta_id: id,
                focus_id: focus,
                source_case_id: None,
                context_key: "chat-1",
                session_reference: "s",
                description: "d",
                relevance_tags: &[],
                entity_ids: &["ent-router"],
                changed_at: None,
                recorded_by_run: "run-1",
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_ticket_handle_resolves_only_an_open_case_of_the_running_focus() {
        let mut conn = mem_outputs_conn().await;
        add_case(&mut conn, FOCUS, "case-1").await;
        add_case(&mut conn, "other-focus", "case-2").await;
        add_case(&mut conn, FOCUS, "case-3").await;

        let h1 = case_handle(FOCUS, "case-1");
        assert_eq!(
            resolve_case_handle(&mut conn, FOCUS, &h1).await.unwrap(),
            "case-1"
        );
        // Another Focus's case: not found for this Focus, under either hash.
        for h in [
            case_handle("other-focus", "case-2"),
            case_handle(FOCUS, "case-2"),
        ] {
            assert!(matches!(
                resolve_case_handle(&mut conn, FOCUS, &h).await,
                Err(HandleError::NotFound("ticket"))
            ));
        }
        // It resolves for the Focus that owns it.
        assert_eq!(
            resolve_case_handle(
                &mut conn,
                "other-focus",
                &case_handle("other-focus", "case-2")
            )
            .await
            .unwrap(),
            "case-2"
        );
        // A closed case is out of reach.
        let c = crate::persistence::case_store::get_case(&mut conn, "case-3")
            .await
            .unwrap()
            .unwrap();
        update_case(
            &mut conn,
            "chat-1",
            "case-3",
            c.version,
            &CaseUpdate {
                state: Some(CaseState::Resolved),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            resolve_case_handle(&mut conn, FOCUS, &case_handle(FOCUS, "case-3")).await,
            Err(HandleError::NotFound(_))
        ));
        // Not a handle at all.
        assert!(matches!(
            resolve_case_handle(&mut conn, FOCUS, "case-1").await,
            Err(HandleError::Malformed('T'))
        ));
        assert!(matches!(
            resolve_case_handle(&mut conn, FOCUS, &delta_handle(FOCUS, "case-1")).await,
            Err(HandleError::Malformed('T'))
        ));
    }

    #[tokio::test]
    async fn a_delta_handle_resolves_only_a_delta_this_focus_recorded() {
        let mut conn = mem_outputs_conn().await;
        add_delta(&mut conn, FOCUS, "delta-1").await;
        add_delta(&mut conn, "other-focus", "delta-2").await;
        assert_eq!(
            resolve_delta_handle(&mut conn, FOCUS, &delta_handle(FOCUS, "delta-1"))
                .await
                .unwrap(),
            "delta-1"
        );
        for h in [
            delta_handle("other-focus", "delta-2"),
            delta_handle(FOCUS, "delta-2"),
        ] {
            assert!(matches!(
                resolve_delta_handle(&mut conn, FOCUS, &h).await,
                Err(HandleError::NotFound(_))
            ));
        }
        assert!(matches!(
            resolve_delta_handle(&mut conn, FOCUS, &case_handle(FOCUS, "delta-1")).await,
            Err(HandleError::Malformed('D'))
        ));
    }

    #[test]
    fn an_entity_handle_is_its_own_kind_and_never_resolves_as_another() {
        let h = entity_handle("tech-support", "ent-1");
        assert_eq!(h, entity_handle("tech-support", "ent-1"));
        assert!(h.starts_with("E-") && h.len() == 2 + HANDLE_CHARS);
        assert!(!h.contains("ent-1"), "the id is not in the handle");
        // Kind and Focus are inside the hash.
        assert_ne!(h[2..], case_handle("tech-support", "ent-1")[2..]);
        assert_ne!(h, entity_handle("other-focus", "ent-1"));
        // The shape test a model's text goes through.
        for ok in [
            h.clone(),
            h.to_lowercase(),
            format!("[{h}]"),
            format!(" {h}\n"),
        ] {
            assert!(looks_like_entity_handle(&ok), "{ok}");
        }
        for not in [
            "none",
            "new",
            "",
            "T-ABCDEFGH",
            "D-ABCDEFGH",
            "E-ABC",
            "E-ABCDEFGHI",
            "E-ABCDEFGI",
            &format!("{h} and more"),
        ] {
            assert!(!looks_like_entity_handle(not), "{not}");
        }
        assert_eq!(canonical(&h, Kind::Ticket), None);
        assert_eq!(canonical(&case_handle("f", "c"), Kind::Entity), None);
    }
}

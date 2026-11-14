// src-tauri/src/persistence/owned_entity_store.rs
//
// items.id=891 stage 2 (decisions.id=507, 940 ruling 2): the typed store layer
// for a Focus-OWNED entity, the device a Focus like Tech Support keeps about the
// user's own equipment. Stage 3's entity writer ops call these functions and
// nothing else; stage 5's device-library commands call the same ones for the
// user's own edits. Every function takes a caller-supplied connection to the
// persona's personal.db (the file is the persona) and the owning Focus, and
// every function is scoped by that Focus: an entity another Focus owns, an
// unowned entity and an archived or purged one are all "no such entity" here.
//
// WHAT IS HERE (and what is deliberately not):
//   find_owned_by_replay_key   the idempotency key of a create (a re-run row)
//   find_owned_by_name         an exact normalized match on name or alias, so a
//                              device named twice is one device
//   create_subject             one entity (type focus_subject, owned, a root) plus
//                              its optional subject_class fact, atomically
//   get_owned_active           the one gate every other function goes through
//   put_owned_fact             one catalogued fact, idempotent, superseding
//   list_owned_active          the Focus's active entities, newest first
//   list_active_facts          an owned entity's active facts, catalogue order
// There is NO delete here. Only entity_purge (stage 4) removes an owned entity,
// its facts or anything else, and a source scan there pins it. A user clearing one
// fact is a purge-module function as well.
//
// FACTS (conductor::device_fields is the closed catalogue). A put validates the
// field name and the value against it, writes the field's own sensitivity and
// abstraction policies (personal_store::create_entity_fact_with_policy_conn), and
// is idempotent: an unchanged active value writes nothing, a changed one
// supersedes through the immutable-history writer, so a replayed row never piles
// up history. The fact's source is the owning Focus slug and its provenance
// persona is the running Persona. cross_persona_export is always false: an owned
// entity never leaves its Persona. An owned entity is never marked user_modified
// (it has no source to refresh from).
//
// ERRORS name the slot or the reason and never a value.
//
// QUERY STYLE: runtime sqlx::query() only, never the query!() macros.

use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use crate::conductor::device_fields::{self, FieldDef, ValueError, SUBJECT_CLASS_FIELD};
use crate::persistence::entity_store::{
    create_owned_entity_conn, get_entity_conn, Entity, EntityFilter, OwnerFilter,
};
use crate::persistence::personal_store::{create_entity_fact_with_policy_conn, PersonalStoreError};
use crate::persistence::savepoint::rollback_to_and_release;

/// The entity_type of every Focus-owned subject.
pub const SUBJECT_ENTITY_TYPE: &str = "focus_subject";

/// Longest device name, in bytes.
pub const NAME_MAX_BYTES: usize = 120;

/// Longest idempotency key ("{run_id}:{action_id}").
pub const REPLAY_KEY_MAX_BYTES: usize = 300;

/// The extra_metadata key that holds the replay key of the row that created the
/// entity.
const CREATED_BY_KEY: &str = "created_by";

#[derive(Debug, Error)]
pub enum OwnedEntityError {
    #[error("{0}")]
    Validation(String),
    /// No such entity for this Focus: missing, another Focus's, unowned, or not
    /// active. The four are indistinguishable on purpose.
    #[error("no such entity for this Focus")]
    NotFound,
    #[error("the field is not usable: {0}")]
    Value(#[from] ValueError),
    #[error("the device catalogue is unusable: {0}")]
    Catalogue(String),
    #[error("{0}")]
    Store(#[from] PersonalStoreError),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// How a find-or-create call resolved a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// A new entity was written.
    Created,
    /// The replay key matched: this row already created the entity.
    Replay,
    /// An existing entity carries this normalized name or alias.
    ByName,
}

/// What a put did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// A fact was written (first value, or a changed value that superseded).
    Written,
    /// The active value was already this one: nothing written.
    Unchanged,
}

fn invalid(msg: impl Into<String>) -> OwnedEntityError {
    OwnedEntityError::Validation(msg.into())
}

fn check_focus(focus: &str) -> Result<(), OwnedEntityError> {
    // The same rule as entity_store::create_owned_entity_conn, which is the
    // authority for the slug shape: a malformed focus matches nothing here.
    if focus.is_empty() || focus.len() > 64 {
        return Err(invalid("focus_id must be a Focus slug"));
    }
    Ok(())
}

/// A display name or alias, normalized for an exact match: trimmed, inner runs of
/// whitespace collapsed to one space, lowercased.
pub fn normalize_name(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A device name as stored: trimmed and inner whitespace collapsed (a newline or
/// tab counts as whitespace and becomes one space, the case the user gave is
/// kept), non-empty, within NAME_MAX_BYTES, no other control character.
fn clean_name(raw: &str) -> Result<String, OwnedEntityError> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err(invalid("the device name is empty"));
    }
    if name.len() > NAME_MAX_BYTES {
        return Err(invalid("the device name is too long"));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(invalid("the device name contains a control character"));
    }
    Ok(name)
}

fn check_replay_key(key: &str) -> Result<(), OwnedEntityError> {
    if key.is_empty() || key.len() > REPLAY_KEY_MAX_BYTES || key.chars().any(|c| c.is_control()) {
        return Err(invalid("the replay key is not usable"));
    }
    Ok(())
}

/// The Focus's owned entity by id, if it is active. The one gate: every write and
/// read below goes through it, so another Focus's entity, an unowned one and a
/// purge-marked one cannot be reached by id.
pub(crate) async fn get_owned_active(
    conn: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<Entity, OwnedEntityError> {
    check_focus(focus)?;
    let entity = get_entity_conn(conn, entity_id)
        .await?
        .filter(|e| e.owner_focus.as_deref() == Some(focus) && e.status == "active")
        .ok_or(OwnedEntityError::NotFound)?;
    Ok(entity)
}

/// The Focus's active owned entity that a row with this replay key created, if
/// any.
pub(crate) async fn find_owned_by_replay_key(
    conn: &mut SqliteConnection,
    focus: &str,
    replay_key: &str,
) -> Result<Option<Entity>, OwnedEntityError> {
    check_focus(focus)?;
    check_replay_key(replay_key)?;
    let owned = list_owned_active(conn, focus).await?;
    Ok(owned.into_iter().find(|e| {
        e.extra_metadata
            .get(CREATED_BY_KEY)
            .and_then(|v| v.as_str())
            == Some(replay_key)
    }))
}

/// The Focus's active owned entity whose name or alias equals `name` once both
/// are normalized. If two match (never created by this layer, which finds before
/// it creates), the older one wins, so the answer is stable.
pub(crate) async fn find_owned_by_name(
    conn: &mut SqliteConnection,
    focus: &str,
    name: &str,
) -> Result<Option<Entity>, OwnedEntityError> {
    check_focus(focus)?;
    let wanted = normalize_name(name);
    if wanted.is_empty() {
        return Err(invalid("the device name is empty"));
    }
    let mut owned = list_owned_active(conn, focus).await?;
    owned.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(owned.into_iter().find(|e| {
        normalize_name(&e.display_name) == wanted
            || e.aliases.iter().any(|a| normalize_name(a) == wanted)
    }))
}

/// The Focus's active owned entities, newest first (ties by id).
pub(crate) async fn list_owned_active(
    conn: &mut SqliteConnection,
    focus: &str,
) -> Result<Vec<Entity>, OwnedEntityError> {
    check_focus(focus)?;
    let mut list = crate::persistence::entity_store::list_entities_conn(
        conn,
        &EntityFilter {
            entity_type: Some(SUBJECT_ENTITY_TYPE.to_owned()),
            status: Some("active".to_owned()),
            owner: OwnerFilter::Focus(focus.to_owned()),
            ..Default::default()
        },
    )
    .await?;
    list.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(list)
}

/// Create one owned subject: the entity (type focus_subject, owned by `focus`, a
/// root) and, when `class` is given, its subject_class fact, in one savepoint, so
/// a refused class leaves no entity behind. `replay_key` is recorded on the entity
/// so a re-run of the creating row finds it (find_owned_by_replay_key). The caller
/// has already looked for an existing entity: this function only writes.
pub(crate) async fn create_subject(
    conn: &mut SqliteConnection,
    focus: &str,
    persona_id: &str,
    name: &str,
    class: Option<&str>,
    replay_key: &str,
) -> Result<String, OwnedEntityError> {
    check_focus(focus)?;
    check_replay_key(replay_key)?;
    let name = clean_name(name)?;
    // Validate the class before anything is written.
    let class_def = match class {
        Some(_) => Some(catalogue_field(SUBJECT_CLASS_FIELD)?),
        None => None,
    };
    let class = match (class, class_def) {
        (Some(c), Some(def)) => Some(device_fields::validate_value(def, c)?),
        _ => None,
    };

    sqlx::query("SAVEPOINT create_owned_subject")
        .execute(&mut *conn)
        .await?;
    let step: Result<String, OwnedEntityError> = async {
        let id = create_owned_entity_conn(
            conn,
            focus,
            SUBJECT_ENTITY_TYPE,
            &name,
            &[],
            Some(serde_json::json!({ CREATED_BY_KEY: replay_key })),
        )
        .await?;
        if let (Some(value), Some(def)) = (class.as_deref(), class_def) {
            write_fact(conn, focus, persona_id, &id, def, value).await?;
        }
        Ok(id)
    }
    .await;
    match step {
        Ok(id) => {
            sqlx::query("RELEASE create_owned_subject")
                .execute(&mut *conn)
                .await?;
            Ok(id)
        }
        Err(e) => {
            rollback_to_and_release(conn, "create_owned_subject").await;
            Err(e)
        }
    }
}

fn catalogue_field(name: &str) -> Result<&'static FieldDef, OwnedEntityError> {
    let catalogue = device_fields::registry().map_err(OwnedEntityError::Catalogue)?;
    catalogue
        .field(name)
        .ok_or(OwnedEntityError::Value(ValueError::UnknownField))
}

/// Write one catalogued fact with the field's own sensitivity and policies. The
/// caller has validated the value and the entity.
async fn write_fact(
    conn: &mut SqliteConnection,
    focus: &str,
    persona_id: &str,
    entity_id: &str,
    def: &FieldDef,
    value: &str,
) -> Result<(), OwnedEntityError> {
    // An empty object, not None: the shared writer would store the text null.
    let metadata = if def.access_credential_flag {
        serde_json::json!({ "access_credential_flag": true })
    } else {
        serde_json::json!({})
    };
    create_entity_fact_with_policy_conn(
        conn,
        Some(entity_id),
        &def.name,
        value,
        &def.sensitivity,
        &def.abstraction_tier2,
        &def.abstraction_tier3,
        focus,
        persona_id,
        false,
        None,
        Some(metadata),
    )
    .await?;
    Ok(())
}

/// Put one catalogued fact on one of the Focus's owned entities. The field must
/// be in the catalogue and the value must fit it (an unknown field, an empty or
/// over-long value fail with an error naming the reason, never the value). The
/// entity must be the Focus's own and active. Idempotent: an active value equal
/// to this one writes nothing.
pub(crate) async fn put_owned_fact(
    conn: &mut SqliteConnection,
    focus: &str,
    persona_id: &str,
    entity_id: &str,
    field: &str,
    value: &str,
) -> Result<PutOutcome, OwnedEntityError> {
    check_focus(focus)?;
    if persona_id.is_empty() {
        return Err(invalid("persona_id is required"));
    }
    let def = catalogue_field(field.trim())?;
    let value = device_fields::validate_value(def, value)?;
    get_owned_active(conn, focus, entity_id).await?;

    let current: Option<String> = sqlx::query_scalar(
        "SELECT CAST(field_value AS TEXT) FROM entity_facts
         WHERE entity_id = ? AND field_name = ? AND valid_until IS NULL",
    )
    .bind(entity_id)
    .bind(&def.name)
    .fetch_optional(&mut *conn)
    .await?;
    if current.as_deref() == Some(value.as_str()) {
        return Ok(PutOutcome::Unchanged);
    }
    write_fact(conn, focus, persona_id, entity_id, def, &value).await?;
    Ok(PutOutcome::Written)
}

/// One active fact of an owned entity, as the device sheet and the derived text
/// read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveFact {
    pub field_name: String,
    pub value: String,
    pub created_at: String,
}

/// An owned entity's active facts that are in the catalogue, in catalogue order.
/// A row whose field the catalogue no longer lists is skipped (a data edit can
/// retire a field without erasing history): it is neither shown nor deleted here.
#[allow(dead_code)] // items.id=891 stage 5 (device sheet) and stage 6 (subject_context) are its first callers
pub(crate) async fn list_active_facts(
    conn: &mut SqliteConnection,
    focus: &str,
    entity_id: &str,
) -> Result<Vec<ActiveFact>, OwnedEntityError> {
    get_owned_active(conn, focus, entity_id).await?;
    let catalogue = device_fields::registry().map_err(OwnedEntityError::Catalogue)?;
    let rows = sqlx::query(
        "SELECT field_name, CAST(field_value AS TEXT) AS value, created_at
         FROM entity_facts WHERE entity_id = ? AND valid_until IS NULL",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut facts: Vec<(usize, ActiveFact)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let field_name: String = row.try_get("field_name")?;
        let Some(pos) = catalogue.position(&field_name) else {
            continue;
        };
        facts.push((
            pos,
            ActiveFact {
                field_name,
                value: row.try_get("value")?,
                created_at: row.try_get("created_at")?,
            },
        ));
    }
    facts.sort_by_key(|(pos, _)| *pos);
    Ok(facts.into_iter().map(|(_, f)| f).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    const FOCUS: &str = "tech-support";
    const OTHER: &str = "other-focus";
    const PERSONA: &str = "persona-1";

    /// In-memory personal.db with the real schema that the owned store touches:
    /// v1 to v3 (entities, entity_facts, flags) and v11 (the owner).
    async fn db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection");
        for schema in [
            include_str!("../../schema/personal_001.sql"),
            include_str!("../../schema/personal_002.sql"),
            include_str!("../../schema/personal_003.sql"),
            include_str!("../../schema/personal_011.sql"),
        ] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
            }
        }
        conn
    }

    async fn make(conn: &mut SqliteConnection, focus: &str, name: &str, key: &str) -> String {
        create_subject(conn, focus, PERSONA, name, None, key)
            .await
            .unwrap()
    }

    async fn fact_rows(
        conn: &mut SqliteConnection,
        entity: &str,
        field: &str,
    ) -> Vec<(String, bool)> {
        sqlx::query(
            "SELECT CAST(field_value AS TEXT) AS v, valid_until IS NULL AS active
             FROM entity_facts WHERE entity_id = ? AND field_name = ? ORDER BY created_at, rowid",
        )
        .bind(entity)
        .bind(field)
        .fetch_all(&mut *conn)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get::<String, _>("v"), r.get::<i64, _>("active") == 1))
        .collect()
    }

    #[tokio::test]
    async fn a_subject_is_owned_typed_a_root_and_remembers_the_row_that_made_it() {
        let mut conn = db().await;
        let id = create_subject(
            &mut conn,
            FOCUS,
            PERSONA,
            "  Living   room Router ",
            None,
            "run-1:find",
        )
        .await
        .unwrap();
        let e = get_owned_active(&mut conn, FOCUS, &id).await.unwrap();
        assert_eq!(
            e.display_name, "Living room Router",
            "whitespace collapsed, case kept"
        );
        assert_eq!(e.entity_type, SUBJECT_ENTITY_TYPE);
        assert_eq!(e.owner_focus.as_deref(), Some(FOCUS));
        assert_eq!(e.parent_entity_id, None);
        assert_eq!(
            find_owned_by_replay_key(&mut conn, FOCUS, "run-1:find")
                .await
                .unwrap()
                .map(|e| e.id),
            Some(id)
        );
        assert!(find_owned_by_replay_key(&mut conn, FOCUS, "run-2:find")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_class_is_stored_as_a_fact_and_a_bad_class_leaves_no_entity() {
        let mut conn = db().await;
        let id = create_subject(
            &mut conn,
            FOCUS,
            PERSONA,
            "Laptop",
            Some("device:endpoint:laptop"),
            "run-1:a",
        )
        .await
        .unwrap();
        assert_eq!(
            fact_rows(&mut conn, &id, SUBJECT_CLASS_FIELD).await,
            [("device:endpoint:laptop".to_owned(), true)]
        );

        let err = create_subject(
            &mut conn,
            FOCUS,
            PERSONA,
            "Phone",
            Some("Device Phone"),
            "run-1:b",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            OwnedEntityError::Value(ValueError::BadSubjectClass)
        ));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 1, "the refused class wrote no entity");
    }

    #[tokio::test]
    async fn names_are_validated_and_errors_never_echo_them() {
        let mut conn = db().await;
        for bad in ["", "   ", "a\0b", &"x".repeat(121)] {
            let err = create_subject(&mut conn, FOCUS, PERSONA, bad, None, "run-1:a")
                .await
                .unwrap_err();
            assert!(matches!(err, OwnedEntityError::Validation(_)), "{bad:?}");
        }
        let err = create_subject(&mut conn, FOCUS, PERSONA, "SECRET\0name", None, "run-1:a")
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("SECRET"));
        for bad_key in ["", "a\nb", &"k".repeat(301)] {
            assert!(
                create_subject(&mut conn, FOCUS, PERSONA, "Router", None, bad_key)
                    .await
                    .is_err()
            );
        }
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn a_name_match_is_exact_after_normalizing_and_scoped_to_the_focus() {
        let mut conn = db().await;
        let mine = make(&mut conn, FOCUS, "Living Room Router", "r:1").await;
        let theirs = make(&mut conn, OTHER, "Living Room Router", "r:2").await;
        sqlx::query("UPDATE entities SET aliases = '[\"Main  Modem\"]' WHERE id = ?")
            .bind(&mine)
            .execute(&mut conn)
            .await
            .unwrap();

        for same in [
            "living room router",
            "  LIVING   room ROUTER ",
            "main modem",
        ] {
            assert_eq!(
                find_owned_by_name(&mut conn, FOCUS, same)
                    .await
                    .unwrap()
                    .map(|e| e.id),
                Some(mine.clone()),
                "{same}"
            );
        }
        // Not a substring or prefix match, and never another Focus's entity.
        for other in ["router", "living room", "living room router 2"] {
            assert!(find_owned_by_name(&mut conn, FOCUS, other)
                .await
                .unwrap()
                .is_none());
        }
        assert_eq!(
            find_owned_by_name(&mut conn, OTHER, "Living Room Router")
                .await
                .unwrap()
                .map(|e| e.id),
            Some(theirs)
        );
        assert!(find_owned_by_name(&mut conn, FOCUS, "   ").await.is_err());
    }

    #[tokio::test]
    async fn only_the_owning_focus_reaches_an_active_owned_entity() {
        let mut conn = db().await;
        let id = make(&mut conn, FOCUS, "Router", "r:1").await;
        let unowned = crate::persistence::entity_store::create_entity_conn(
            &mut conn,
            "person",
            "Sam",
            &[],
            None,
            None,
        )
        .await
        .unwrap();

        assert!(get_owned_active(&mut conn, FOCUS, &id).await.is_ok());
        for (focus, entity) in [
            (OTHER, id.as_str()),
            (FOCUS, unowned.as_str()),
            (FOCUS, "missing"),
        ] {
            assert!(
                matches!(
                    get_owned_active(&mut conn, focus, entity).await,
                    Err(OwnedEntityError::NotFound)
                ),
                "{focus} / {entity}"
            );
        }
        // A purge-marked or archived entity is gone to every reader here.
        for status in ["user_deleted", "archived"] {
            sqlx::query("UPDATE entities SET status = ? WHERE id = ?")
                .bind(status)
                .bind(&id)
                .execute(&mut conn)
                .await
                .unwrap();
            assert!(matches!(
                get_owned_active(&mut conn, FOCUS, &id).await,
                Err(OwnedEntityError::NotFound)
            ));
            assert!(list_owned_active(&mut conn, FOCUS)
                .await
                .unwrap()
                .is_empty());
            assert!(find_owned_by_name(&mut conn, FOCUS, "Router")
                .await
                .unwrap()
                .is_none());
            assert!(find_owned_by_replay_key(&mut conn, FOCUS, "r:1")
                .await
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn a_put_writes_the_fields_own_policy_and_is_idempotent() {
        let mut conn = db().await;
        let id = make(&mut conn, FOCUS, "Router", "r:1").await;

        assert_eq!(
            put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "serial_number", "SN-1")
                .await
                .unwrap(),
            PutOutcome::Written
        );
        assert_eq!(
            put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "serial_number", "  SN-1 ")
                .await
                .unwrap(),
            PutOutcome::Unchanged,
            "the same value after trimming writes nothing"
        );
        assert_eq!(fact_rows(&mut conn, &id, "serial_number").await.len(), 1);

        // The policy columns and the provenance are the catalogue's, not the defaults.
        let row = sqlx::query(
            "SELECT sensitivity, abstraction_tier2, abstraction_tier3, source,
                    source_persona_id, cross_persona_export, extra_metadata
             FROM entity_facts WHERE entity_id = ? AND field_name = 'serial_number'",
        )
        .bind(&id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("sensitivity"), "personal");
        assert_eq!(row.get::<String, _>("abstraction_tier2"), "not_permitted");
        assert_eq!(row.get::<String, _>("abstraction_tier3"), "not_permitted");
        assert_eq!(row.get::<String, _>("source"), FOCUS);
        assert_eq!(row.get::<String, _>("source_persona_id"), PERSONA);
        assert_eq!(row.get::<i64, _>("cross_persona_export"), 0);
        let meta: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("extra_metadata")).unwrap();
        assert_eq!(meta["access_credential_flag"], true);

        // A general field is written with pass, and no credential flag.
        put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "make", "Netgear")
            .await
            .unwrap();
        let row = sqlx::query(
            "SELECT sensitivity, abstraction_tier2, extra_metadata FROM entity_facts
             WHERE entity_id = ? AND field_name = 'make'",
        )
        .bind(&id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("sensitivity"), "general");
        assert_eq!(row.get::<String, _>("abstraction_tier2"), "pass");
        assert_eq!(row.get::<String, _>("extra_metadata"), "{}");
    }

    #[tokio::test]
    async fn a_changed_value_supersedes_and_keeps_history() {
        let mut conn = db().await;
        let id = make(&mut conn, FOCUS, "Router", "r:1").await;
        put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "firmware_version", "1.0")
            .await
            .unwrap();
        assert_eq!(
            put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "firmware_version", "2.0")
                .await
                .unwrap(),
            PutOutcome::Written
        );
        assert_eq!(
            fact_rows(&mut conn, &id, "firmware_version").await,
            [("1.0".to_owned(), false), ("2.0".to_owned(), true)]
        );
        // Putting the old value back is a change too.
        put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "firmware_version", "1.0")
            .await
            .unwrap();
        assert_eq!(fact_rows(&mut conn, &id, "firmware_version").await.len(), 3);
    }

    #[tokio::test]
    async fn an_unknown_field_or_bad_value_or_wrong_entity_writes_nothing() {
        let mut conn = db().await;
        let id = make(&mut conn, FOCUS, "Router", "r:1").await;
        let theirs = make(&mut conn, OTHER, "Camera", "r:2").await;

        for (focus, entity, field, value, what) in [
            (
                FOCUS,
                id.as_str(),
                "wifi_password",
                "hunter2",
                "a credential field",
            ),
            (
                FOCUS,
                id.as_str(),
                "notes",
                "free text",
                "a field outside the catalogue",
            ),
            (FOCUS, id.as_str(), "make", "   ", "an empty value"),
            (
                FOCUS,
                id.as_str(),
                "make",
                &"x".repeat(81),
                "an over-long value",
            ),
            (FOCUS, id.as_str(), "make", "a\nb", "a control character"),
            (
                FOCUS,
                id.as_str(),
                "subject_class",
                "Not A Class",
                "a bad class",
            ),
            (
                FOCUS,
                theirs.as_str(),
                "make",
                "Netgear",
                "another Focus's entity",
            ),
            (
                OTHER,
                id.as_str(),
                "make",
                "Netgear",
                "another Focus asking",
            ),
            (FOCUS, "missing", "make", "Netgear", "a missing entity"),
        ] {
            assert!(
                put_owned_fact(&mut conn, focus, PERSONA, entity, field, value)
                    .await
                    .is_err(),
                "{what} must be refused"
            );
        }
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_facts")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(n, 0, "every refusal wrote nothing");

        let msg = put_owned_fact(&mut conn, FOCUS, PERSONA, &id, "wifi_password", "hunter2")
            .await
            .unwrap_err()
            .to_string();
        assert!(!msg.contains("hunter2"), "an error never names the value");
    }

    #[tokio::test]
    async fn active_facts_come_back_in_catalogue_order_and_skip_unlisted_fields() {
        let mut conn = db().await;
        let id = make(&mut conn, FOCUS, "Router", "r:1").await;
        for (field, value) in [
            ("known_issues", "drops at night"),
            ("make", "Netgear"),
            ("firmware_version", "2.0"),
        ] {
            put_owned_fact(&mut conn, FOCUS, PERSONA, &id, field, value)
                .await
                .unwrap();
        }
        // A fact whose field the catalogue does not list (retired by a data edit).
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source_persona_id, created_at)
             VALUES ('legacy', ?, 'retired_field', 'x', 'general', ?, 't')",
        )
        .bind(&id)
        .bind(PERSONA)
        .execute(&mut conn)
        .await
        .unwrap();

        let facts = list_active_facts(&mut conn, FOCUS, &id).await.unwrap();
        let names: Vec<&str> = facts.iter().map(|f| f.field_name.as_str()).collect();
        assert_eq!(names, ["make", "firmware_version", "known_issues"]);
        assert!(matches!(
            list_active_facts(&mut conn, OTHER, &id).await,
            Err(OwnedEntityError::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_owned_list_is_newest_first_and_scoped() {
        let mut conn = db().await;
        let a = make(&mut conn, FOCUS, "A", "r:1").await;
        let _other = make(&mut conn, OTHER, "Z", "r:2").await;
        // created_at has millisecond resolution: set distinct stamps.
        sqlx::query("UPDATE entities SET created_at = '2026-01-01T00:00:00Z' WHERE id = ?")
            .bind(&a)
            .execute(&mut conn)
            .await
            .unwrap();
        let b = make(&mut conn, FOCUS, "B", "r:3").await;
        sqlx::query("UPDATE entities SET created_at = '2026-02-01T00:00:00Z' WHERE id = ?")
            .bind(&b)
            .execute(&mut conn)
            .await
            .unwrap();
        let names: Vec<String> = list_owned_active(&mut conn, FOCUS)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.display_name)
            .collect();
        assert_eq!(names, ["B", "A"]);
    }

    #[test]
    fn this_module_has_no_delete() {
        // Only entity_purge removes an owned entity or a fact (stage 4). The scan
        // reads this file's own non-test source.
        let src = include_str!("owned_entity_store.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["DELETE FROM", "DROP TABLE", "TRUNCATE"] {
            let code: Vec<&str> = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(forbidden))
                .collect();
            assert!(
                code.is_empty(),
                "owned_entity_store must not contain {forbidden}"
            );
        }
    }
}

// src-tauri/src/persistence/group_store.rs
//
// group.db connection layer -- items.id=283 (266a) -- plus document CRUD
// and permission enforcement -- items.id=285 (fourth of six items.id=266
// sub-items). items.id=283 built the self-healing opener with no caller;
// this item is its first real one, below open_group_db's own definition.
//
// Path: {QR_DATA_ROOT}/groups/{persona_id}/{group_id}/group.db -- see
// migrations.rs::migrate_group_db's own doc comment for why this is NOT
// nested under users/{user_id}/... the way personal.db/outputs.db are.
// One group.db PER (persona_id, group_id) PAIR -- each member has their
// own local copy, not a single shared file -- so persona_id below is both
// "whose local copy to open" and "the identity every permission check is
// evaluated against": a persona can only ever open its own copy, so no
// separate requesting_persona_id parameter is needed anywhere in this file.
//
// SELF-HEAL FROM DAY ONE (items.id=275/278 precedent): open_group_db below
// follows the exact check-then-migrate-then-connect pattern
// personal_store.rs::open_personal_db and output_store.rs::open_outputs_db
// already establish, rather than a bare create_if_missing(false) connect
// with no migration call anywhere in the real call path -- the failure
// mode those two items spent three dispatches fixing across nine other
// stores, not repeated here.
//
// PERMISSION ENFORCEMENT (design doc Section 2.2, repeated at every check
// site below since it's easy to mistake for something stronger): every
// owner/write/read_only check in this file is an APP-LAYER POLICY check on
// top of data every member can already technically decrypt with the group
// key -- comparable to a shared Google Doc's sharing settings, NOT the
// same security class as personal.db's cross-account isolation. These
// functions record and respect trust-based sharing intent; they do not
// and cannot cryptographically prevent a group-key holder from reading
// group.db's raw rows outside this API.
//
// content_ref STORAGE SHAPE: documents.content_ref holds literal document
// text inline (same shape as outputs.content), not a filesystem path,
// despite the "_ref" name and the schema's "pointer to actual content
// storage" comment. See create_document's own doc comment for the full
// reasoning -- short version: design doc Section 2.4's sync security note
// only holds ("the file synced... is the same encrypted-at-rest SQLCipher
// file", "SQLCipher's page-level encryption" makes diffing unnecessary) if
// document content lives inside the encrypted group.db itself; a separate
// on-disk file would need its own independent at-rest encryption and sync
// path that Section 2.4 never describes, and no precedent for per-entity
// content-file storage exists anywhere else in this codebase.
//
// TWO AMBIGUOUS DESIGN CALLS (both flagged as genuinely underspecified by
// the source design doc -- see get_document's and update_document's own
// doc comments for the full reasoning on each):
//   - Read access requires an explicit grant (owner or a document_
//     permissions row) -- holding the group key alone is not sufficient.
//   - Checkout is a separate, optional UX-level lock; update_document does
//     NOT require or check it.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-small-
// encrypted-DB topology, no static DATABASE_URL).

use std::path::PathBuf;

use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum GroupStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] crate::persistence::migrations::MigrationError),
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Document '{0}' not found")]
    DocumentNotFound(String),
    #[error("Persona '{0}' does not have the required permission on document '{1}'")]
    PermissionDenied(String, String),
    #[error("Document '{0}' is already checked out by a different persona")]
    AlreadyCheckedOut(String),
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn get_group_db_path(persona_id: &str, group_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("groups")
        .join(persona_id)
        .join(group_id)
        .join("group.db")
}

// ---------------------------------------------------------------------------
// DB opener
// ---------------------------------------------------------------------------

/// Open group.db with SQLCipher key, self-healing (migrating) a never-yet-
/// created file on first access. Caller supplies bare hex; wrapped in
/// SQLCipher x'...' syntax by connect_options_encrypted.
///
/// key_hex boundary: same convention as personal_store.rs/output_store.rs --
/// this is a plain library function (not a #[tauri::command]), key_hex is
/// derived by the command-layer caller from auth::registry::GroupKeyRegistry,
/// not accepted as a bare IPC parameter from the frontend.
pub(crate) async fn open_group_db(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, GroupStoreError> {
    let db_path = get_group_db_path(persona_id, group_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_group_db(persona_id, group_id, key_hex).await?;
    }

    let conn = crate::providers::utils::connect_options_encrypted(&db_path, key_hex)
        .create_if_missing(false)
        .connect()
        .await?;

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// One documents row. `content` maps to the `content_ref` column -- see
/// this file's own header (content_ref STORAGE SHAPE) for why this item
/// stores literal text there rather than an external pointer.
#[derive(Debug, Clone)]
pub struct DocumentRecord {
    pub id: String,
    pub title: String,
    pub content: String,
    pub owner_persona_id: String,
    pub checked_out_by_persona_id: Option<String>,
    pub checked_out_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub extra_metadata: String,
}

fn row_to_document_record(r: &sqlx::sqlite::SqliteRow) -> Result<DocumentRecord, sqlx::Error> {
    Ok(DocumentRecord {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        // content_ref is nullable in the schema; create_document always
        // populates it, so NULL only reachable via a row this module never
        // wrote -- default to empty rather than erroring on read.
        content: r
            .try_get::<Option<String>, _>("content_ref")?
            .unwrap_or_default(),
        owner_persona_id: r.try_get("owner_persona_id")?,
        checked_out_by_persona_id: r.try_get("checked_out_by_persona_id")?,
        checked_out_at: r.try_get("checked_out_at")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        extra_metadata: r.try_get("extra_metadata")?,
    })
}

// ---------------------------------------------------------------------------
// Permission-check helpers
// ---------------------------------------------------------------------------
//
// APP-LAYER POLICY CHECKS, NOT A SECURITY BOUNDARY (design doc Section 2.2,
// restated at every site -- see this file's own header for the full note):
// anyone holding the group's symmetric key can already decrypt every raw
// row in this database. These helpers enforce trust-based sharing intent
// among people who already share that key -- they do not, and structurally
// cannot, prevent a group-key holder from bypassing this API and reading
// group.db directly.

async fn fetch_document_owner_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
) -> Result<Option<String>, GroupStoreError> {
    let row = sqlx::query("SELECT owner_persona_id FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(match row {
        Some(r) => Some(r.try_get("owner_persona_id")?),
        None => None,
    })
}

async fn fetch_permission_tier_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<Option<String>, GroupStoreError> {
    let row = sqlx::query(
        "SELECT tier FROM document_permissions WHERE document_id = ? AND persona_id = ?",
    )
    .bind(document_id)
    .bind(persona_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some(r) => Some(r.try_get("tier")?),
        None => None,
    })
}

/// Read access -- owner OR any document_permissions row (either tier).
///
/// AMBIGUOUS DESIGN CALL, resolved here: does holding the group key alone
/// entitle reading every document, or only documents explicitly granted?
/// Chose explicit-grant-required. Section 2.2's own analogy is "comparable
/// to a shared Google Doc's sharing settings" -- a Google Doc's sharing
/// settings gate who can open *that specific doc*, not "anyone with access
/// to the Drive can read every file in it." If mere group-key possession
/// were sufficient, document_permissions would have no read-side purpose
/// at all (only write would matter), which doesn't fit a table explicitly
/// described as governing three tiers including read-only.
async fn require_read_access_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<(), GroupStoreError> {
    let owner = fetch_document_owner_conn(conn, document_id)
        .await?
        .ok_or_else(|| GroupStoreError::DocumentNotFound(document_id.to_owned()))?;
    if owner == persona_id {
        return Ok(());
    }
    match fetch_permission_tier_conn(conn, document_id, persona_id).await? {
        Some(_) => Ok(()),
        None => Err(GroupStoreError::PermissionDenied(
            persona_id.to_owned(),
            document_id.to_owned(),
        )),
    }
}

/// Write access -- owner OR 'write' tier. read_only and no-grant are both
/// rejected. This is the core enforcement check for update_document.
async fn require_write_access_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<(), GroupStoreError> {
    let owner = fetch_document_owner_conn(conn, document_id)
        .await?
        .ok_or_else(|| GroupStoreError::DocumentNotFound(document_id.to_owned()))?;
    if owner == persona_id {
        return Ok(());
    }
    match fetch_permission_tier_conn(conn, document_id, persona_id).await? {
        Some(ref t) if t == "write" => Ok(()),
        _ => Err(GroupStoreError::PermissionDenied(
            persona_id.to_owned(),
            document_id.to_owned(),
        )),
    }
}

/// Owner-only access. Returns the confirmed owner id on success (always
/// equal to `persona_id`) so grant_permission_conn can reuse it for the
/// owner-conflict check without a second query.
async fn require_owner_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<String, GroupStoreError> {
    let owner = fetch_document_owner_conn(conn, document_id)
        .await?
        .ok_or_else(|| GroupStoreError::DocumentNotFound(document_id.to_owned()))?;
    if owner == persona_id {
        Ok(owner)
    } else {
        Err(GroupStoreError::PermissionDenied(
            persona_id.to_owned(),
            document_id.to_owned(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

async fn create_document_conn(
    conn: &mut SqliteConnection,
    owner_persona_id: &str,
    title: &str,
    content: &str,
) -> Result<String, GroupStoreError> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO documents (id, title, content_ref, owner_persona_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(title)
    .bind(content)
    .bind(owner_persona_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *conn)
    .await?;

    Ok(id)
}

/// Create a new document. The creating Persona becomes owner by default
/// (design doc Section 2.4: "each document has exactly one owner at a
/// time") -- caller supplies owner_persona_id explicitly, matching
/// output_store::save_output's caller-supplies-everything shape; this
/// function does not itself enforce persona_id == owner_persona_id, since
/// nothing in the design doc restricts who may create a document naming
/// whom as owner and app-layer trust is already established by group-key
/// possession. Returns the new document's id.
///
/// content is stored literally in documents.content_ref -- see this file's
/// own header (content_ref STORAGE SHAPE) for the full reasoning on why
/// this is inline text, not a filesystem path, despite the column's "_ref"
/// name and "pointer to actual content storage" schema comment.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn create_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    owner_persona_id: &str,
    title: &str,
    content: &str,
) -> Result<String, GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    create_document_conn(&mut conn, owner_persona_id, title, content).await
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

async fn get_document_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<DocumentRecord, GroupStoreError> {
    require_read_access_conn(conn, document_id, persona_id).await?;

    let row = sqlx::query("SELECT * FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;

    row_to_document_record(&row).map_err(GroupStoreError::Database)
}

/// Fetch a document's content + metadata for `persona_id`. Requires
/// owner-or-any-permission-row -- see require_read_access_conn's own doc
/// comment for the read-access design call and its reasoning. No tier
/// distinction for reading: write and read_only both grant read access.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn get_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
) -> Result<DocumentRecord, GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    get_document_conn(&mut conn, document_id, persona_id).await
}

async fn list_documents_conn(
    conn: &mut SqliteConnection,
    persona_id: &str,
) -> Result<Vec<DocumentRecord>, GroupStoreError> {
    let rows = sqlx::query(
        "SELECT DISTINCT d.* FROM documents d
         LEFT JOIN document_permissions p
             ON p.document_id = d.id AND p.persona_id = ?
         WHERE d.owner_persona_id = ? OR p.persona_id IS NOT NULL
         ORDER BY d.updated_at DESC",
    )
    .bind(persona_id)
    .bind(persona_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_document_record(r).map_err(GroupStoreError::Database)?);
    }
    Ok(out)
}

/// List documents `persona_id` can see in this group.db -- same visibility
/// rule as get_document (owner or any permission row), not every document
/// in the file.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn list_documents(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
) -> Result<Vec<DocumentRecord>, GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    list_documents_conn(&mut conn, persona_id).await
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

async fn update_document_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
    content: &str,
) -> Result<(), GroupStoreError> {
    require_write_access_conn(conn, document_id, persona_id).await?;

    let now = crate::providers::utils::now();
    sqlx::query("UPDATE documents SET content_ref = ?, updated_at = ? WHERE id = ?")
        .bind(content)
        .bind(&now)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Write new content to a document. Requires owner or 'write' tier --
/// 'read_only' and no-grant are both rejected. This is the core
/// enforcement check for this item (see require_write_access_conn).
///
/// AMBIGUOUS DESIGN CALL, resolved here: must a document be checked out
/// before update_document succeeds? Chose no -- checkout is a separate,
/// optional UX-level lock that this function does not check or require.
/// Design doc Section 2.3 (the actual edit-behavior section: canon-update
/// vs. fork-to-personal) never mentions checkout as a precondition.
/// Checkout only enters the picture via items.id=210's resolution about
/// *stale-lock handling policy* (manual-only, no auto-timeout) -- that's a
/// policy about how an already-adopted lock UX degrades, not a mandate
/// that locking is required before every write. Hard-requiring checkout
/// here would add a new failure mode/UX step the source docs never
/// specify, and would conflict with Section 2.4's single-writer-per-
/// document sync model letting owner/write-tier holders push quick canon
/// updates without extra ceremony.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn update_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    content: &str,
) -> Result<(), GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    update_document_conn(&mut conn, document_id, persona_id, content).await
}

// ---------------------------------------------------------------------------
// Grant / Revoke
// ---------------------------------------------------------------------------

const VALID_TIERS: &[&str] = &["write", "read_only"];

async fn grant_permission_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    acting_persona_id: &str,
    grantee_persona_id: &str,
    tier: &str,
) -> Result<(), GroupStoreError> {
    if !VALID_TIERS.contains(&tier) {
        return Err(GroupStoreError::Validation(format!(
            "Invalid tier '{tier}'. Must be one of: {}",
            VALID_TIERS.join(", ")
        )));
    }

    let owner = require_owner_conn(conn, document_id, acting_persona_id).await?;
    if grantee_persona_id == owner {
        return Err(GroupStoreError::Validation(format!(
            "Persona '{grantee_persona_id}' is already the owner of document \
             '{document_id}' -- cannot also hold a document_permissions row"
        )));
    }

    let now = crate::providers::utils::now();
    sqlx::query(
        "INSERT INTO document_permissions (document_id, persona_id, tier, granted_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(document_id, persona_id)
         DO UPDATE SET tier = excluded.tier, granted_at = excluded.granted_at",
    )
    .bind(document_id)
    .bind(grantee_persona_id)
    .bind(tier)
    .bind(&now)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Grant (or promote/demote) `grantee_persona_id`'s tier on a document.
/// Owner-exclusive -- design doc Section 2.2 says "any member can be
/// promoted or demoted between all three tiers by whoever currently has
/// owner-level authority over that document"; read as owner-exclusive
/// (task brief's own instruction, absent contradicting evidence -- none
/// found: a write-tier holder is never described as able to grant/revoke
/// anywhere in the design doc). tier must be 'write' or 'read_only' --
/// 'owner' is not a valid document_permissions value (schema CHECK
/// constraint; validated here first for a clean error). Rejects granting
/// to the current owner -- a persona must not simultaneously be owner and
/// hold a permission row (schema header's documented invariant,
/// deliberately left to application logic; this is the only entry point
/// that could create that conflict). ON CONFLICT DO UPDATE makes
/// re-granting a different tier a clean promotion/demotion, not a second
/// row or an error.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn grant_permission(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    grantee_persona_id: &str,
    tier: &str,
) -> Result<(), GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    grant_permission_conn(&mut conn, document_id, persona_id, grantee_persona_id, tier).await
}

async fn revoke_permission_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    acting_persona_id: &str,
    grantee_persona_id: &str,
) -> Result<(), GroupStoreError> {
    require_owner_conn(conn, document_id, acting_persona_id).await?;

    sqlx::query("DELETE FROM document_permissions WHERE document_id = ? AND persona_id = ?")
        .bind(document_id)
        .bind(grantee_persona_id)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// Revoke `grantee_persona_id`'s permission on a document. Owner-exclusive
/// (see grant_permission's own doc comment). Idempotent -- revoking a
/// grant that doesn't exist is a no-op, not an error, matching
/// output_store.rs::delete_output_conn's idempotent-delete convention.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn revoke_permission(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
    grantee_persona_id: &str,
) -> Result<(), GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    revoke_permission_conn(&mut conn, document_id, persona_id, grantee_persona_id).await
}

// ---------------------------------------------------------------------------
// Checkout / Force-unlock
// ---------------------------------------------------------------------------

async fn checkout_document_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<(), GroupStoreError> {
    // Owner-or-write bar: a read_only holder can't usefully hold an edit
    // lock (they can't call update_document regardless), and letting them
    // claim one anyway could strand real editors without any way to know
    // why they can't write.
    require_write_access_conn(conn, document_id, persona_id).await?;

    let row = sqlx::query("SELECT checked_out_by_persona_id FROM documents WHERE id = ?")
        .bind(document_id)
        .fetch_one(&mut *conn)
        .await?;
    let current: Option<String> = row.try_get("checked_out_by_persona_id")?;

    if let Some(holder) = &current {
        if holder != persona_id {
            return Err(GroupStoreError::AlreadyCheckedOut(document_id.to_owned()));
        }
    }

    let now = crate::providers::utils::now();
    sqlx::query(
        "UPDATE documents SET checked_out_by_persona_id = ?, checked_out_at = ? WHERE id = ?",
    )
    .bind(persona_id)
    .bind(&now)
    .bind(document_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Check out a document for editing. Fails with AlreadyCheckedOut if held
/// by a *different* persona; re-checkout by the same persona already
/// holding it is a no-op success (refreshes checked_out_at). Does not gate
/// update_document -- see update_document's own doc comment for the
/// checkout-not-required-for-write design call and its reasoning. Manual
/// lock only, no automatic timeout/expiry (items.id=210's resolution --
/// force_unlock_document below is the only release mechanism besides the
/// original holder checking out again... there is no "check-in" call,
/// since force_unlock is unconditional and sufficient).
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn checkout_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
) -> Result<(), GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    checkout_document_conn(&mut conn, document_id, persona_id).await
}

async fn force_unlock_document_conn(
    conn: &mut SqliteConnection,
    document_id: &str,
    persona_id: &str,
) -> Result<(), GroupStoreError> {
    // Same owner-or-write bar as checkout -- a read_only holder can't
    // manipulate a lock they were never eligible to hold in the first
    // place.
    require_write_access_conn(conn, document_id, persona_id).await?;

    sqlx::query(
        "UPDATE documents
         SET checked_out_by_persona_id = NULL, checked_out_at = NULL
         WHERE id = ?",
    )
    .bind(document_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Unconditionally clear a document's checkout, regardless of who (if
/// anyone) currently holds it. items.id=210's resolution: stale-lock
/// handling is manual force-unlock only, no automatic timeout -- so there
/// is deliberately no expiry/heartbeat logic here, just a deliberate
/// clear of both fields. No-op (not an error) if the document was not
/// checked out.
#[allow(dead_code)] // items.id=285: ahead of its first real caller (no IPC layer yet, items.id=283/284 precedent)
pub async fn force_unlock_document(
    persona_id: &str,
    group_id: &str,
    key_hex: &str,
    document_id: &str,
) -> Result<(), GroupStoreError> {
    let mut conn = open_group_db(persona_id, group_id, key_hex).await?;
    force_unlock_document_conn(&mut conn, document_id, persona_id).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use sqlx::sqlite::SqliteConnectOptions;

    const GROUP_SCHEMA: &str = include_str!("../../schema/group_001.sql");

    /// In-memory group.db schema, for fast tests of the *_conn functions
    /// that don't need a real SQLCipher-encrypted file. Same pattern
    /// output_store.rs's own test module uses for outputs_001.sql.
    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for stmt in parse_statements(GROUP_SCHEMA) {
            sqlx::query(&stmt)
                .execute(&mut conn)
                .await
                .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
        }
        conn
    }

    // -- create / get -------------------------------------------------------

    #[tokio::test]
    async fn create_then_get_document_round_trips_for_owner() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Family Budget", "initial content")
            .await
            .expect("create_document_conn failed");

        let doc = get_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .expect("owner must be able to read their own document");

        assert_eq!(doc.title, "Family Budget");
        assert_eq!(doc.content, "initial content");
        assert_eq!(doc.owner_persona_id, "owner-1");
        assert!(doc.checked_out_by_persona_id.is_none());
    }

    #[tokio::test]
    async fn get_document_rejects_persona_with_no_permission_and_not_owner() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Policy Doc", "v1")
            .await
            .unwrap();

        let result = get_document_conn(&mut conn, &doc_id, "stranger").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    #[tokio::test]
    async fn get_document_allows_read_only_tier_holder() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1", "read_only")
            .await
            .unwrap();

        let doc = get_document_conn(&mut conn, &doc_id, "reader-1")
            .await
            .expect("read_only tier must be able to read");
        assert_eq!(doc.content, "v1");
    }

    #[tokio::test]
    async fn get_document_on_nonexistent_id_is_document_not_found() {
        let mut conn = test_db().await;
        let result = get_document_conn(&mut conn, "does-not-exist", "anyone").await;
        assert!(matches!(result, Err(GroupStoreError::DocumentNotFound(_))));
    }

    // -- update ---------------------------------------------------------------

    #[tokio::test]
    async fn update_document_succeeds_for_owner() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        update_document_conn(&mut conn, &doc_id, "owner-1", "v2")
            .await
            .expect("owner update must succeed");

        let doc = get_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .unwrap();
        assert_eq!(doc.content, "v2");
    }

    #[tokio::test]
    async fn update_document_succeeds_for_write_tier_holder() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "editor-1", "write")
            .await
            .unwrap();

        update_document_conn(&mut conn, &doc_id, "editor-1", "v2")
            .await
            .expect("write tier update must succeed");

        let doc = get_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .unwrap();
        assert_eq!(doc.content, "v2");
    }

    #[tokio::test]
    async fn update_document_rejects_read_only_tier_holder() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1", "read_only")
            .await
            .unwrap();

        let result = update_document_conn(&mut conn, &doc_id, "reader-1", "v2").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));

        let doc = get_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .unwrap();
        assert_eq!(doc.content, "v1", "rejected update must not change content");
    }

    #[tokio::test]
    async fn update_document_rejects_persona_with_no_permission() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        let result = update_document_conn(&mut conn, &doc_id, "stranger", "v2").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    // -- list -----------------------------------------------------------------

    #[tokio::test]
    async fn list_documents_returns_only_owned_and_granted_documents() {
        let mut conn = test_db().await;
        let owned_id = create_document_conn(&mut conn, "persona-1", "Owned", "a")
            .await
            .unwrap();
        let granted_id = create_document_conn(&mut conn, "owner-2", "Granted", "b")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &granted_id, "owner-2", "persona-1", "read_only")
            .await
            .unwrap();
        let _hidden_id = create_document_conn(&mut conn, "owner-3", "Hidden", "c")
            .await
            .unwrap();

        let docs = list_documents_conn(&mut conn, "persona-1")
            .await
            .expect("list_documents_conn failed");
        let ids: std::collections::HashSet<_> = docs.iter().map(|d| d.id.clone()).collect();

        assert!(ids.contains(&owned_id));
        assert!(ids.contains(&granted_id));
        assert_eq!(
            ids.len(),
            2,
            "a document with no owner/grant relation to persona-1 must not appear"
        );
    }

    // -- grant / revoke ---------------------------------------------------------

    #[tokio::test]
    async fn grant_permission_rejects_non_owner_including_write_tier_holder() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "editor-1", "write")
            .await
            .unwrap();

        // A write-tier holder must NOT be able to grant permissions --
        // owner-exclusive per GROUP_DB_DESIGN Section 2.2.
        let result =
            grant_permission_conn(&mut conn, &doc_id, "editor-1", "someone-else", "read_only")
                .await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    #[tokio::test]
    async fn grant_permission_rejects_granting_to_the_current_owner() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        let result = grant_permission_conn(&mut conn, &doc_id, "owner-1", "owner-1", "write").await;
        assert!(matches!(result, Err(GroupStoreError::Validation(_))));
    }

    #[tokio::test]
    async fn grant_permission_rejects_invalid_tier_value() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        let result = grant_permission_conn(&mut conn, &doc_id, "owner-1", "someone", "owner").await;
        assert!(matches!(result, Err(GroupStoreError::Validation(_))));
    }

    #[tokio::test]
    async fn grant_permission_promotes_and_demotes_an_existing_grant() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "member-1", "read_only")
            .await
            .unwrap();

        assert!(matches!(
            update_document_conn(&mut conn, &doc_id, "member-1", "v2").await,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));

        // Promote to write.
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "member-1", "write")
            .await
            .unwrap();
        update_document_conn(&mut conn, &doc_id, "member-1", "v2")
            .await
            .expect("promoted member must be able to write");
    }

    #[tokio::test]
    async fn revoke_permission_removes_subsequent_access() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1", "read_only")
            .await
            .unwrap();
        get_document_conn(&mut conn, &doc_id, "reader-1")
            .await
            .expect("must be able to read before revoke");

        revoke_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1")
            .await
            .expect("revoke must succeed");

        let result = get_document_conn(&mut conn, &doc_id, "reader-1").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    #[tokio::test]
    async fn revoke_permission_rejects_non_owner() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1", "read_only")
            .await
            .unwrap();

        let result = revoke_permission_conn(&mut conn, &doc_id, "reader-1", "reader-1").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    #[tokio::test]
    async fn revoke_permission_on_a_nonexistent_grant_is_a_noop_not_an_error() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        let result = revoke_permission_conn(&mut conn, &doc_id, "owner-1", "never-granted").await;
        assert!(
            result.is_ok(),
            "revoking a nonexistent grant must not error"
        );
    }

    // -- checkout / force-unlock ------------------------------------------------

    #[tokio::test]
    async fn checkout_document_blocks_a_second_checkout_by_a_different_persona() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "editor-1", "write")
            .await
            .unwrap();

        checkout_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .expect("first checkout must succeed");

        let result = checkout_document_conn(&mut conn, &doc_id, "editor-1").await;
        assert!(matches!(result, Err(GroupStoreError::AlreadyCheckedOut(_))));
    }

    #[tokio::test]
    async fn checkout_document_by_the_same_persona_twice_is_a_noop_not_an_error() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();

        checkout_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .unwrap();
        let result = checkout_document_conn(&mut conn, &doc_id, "owner-1").await;
        assert!(
            result.is_ok(),
            "re-checkout by the same holder must not error"
        );
    }

    #[tokio::test]
    async fn checkout_document_rejects_read_only_tier_holder() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "reader-1", "read_only")
            .await
            .unwrap();

        let result = checkout_document_conn(&mut conn, &doc_id, "reader-1").await;
        assert!(matches!(
            result,
            Err(GroupStoreError::PermissionDenied(_, _))
        ));
    }

    #[tokio::test]
    async fn force_unlock_document_clears_an_existing_checkout_unconditionally() {
        let mut conn = test_db().await;
        let doc_id = create_document_conn(&mut conn, "owner-1", "Doc", "v1")
            .await
            .unwrap();
        grant_permission_conn(&mut conn, &doc_id, "owner-1", "editor-1", "write")
            .await
            .unwrap();
        checkout_document_conn(&mut conn, &doc_id, "editor-1")
            .await
            .unwrap();

        // Force-unlock by a persona other than the current lock holder.
        force_unlock_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .expect("force unlock must succeed");

        let doc = get_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .unwrap();
        assert!(doc.checked_out_by_persona_id.is_none());
        assert!(doc.checked_out_at.is_none());

        // Lock is fully released -- someone else can now check out cleanly.
        checkout_document_conn(&mut conn, &doc_id, "owner-1")
            .await
            .expect("checkout after force-unlock must succeed");
    }

    // -- real encrypted-file round trip (public API, not just _conn) ------------

    /// Proves the full path through open_group_db (a real, temp-dir-backed
    /// encrypted SQLCipher file) works end-to-end, not just the in-memory
    /// schema the *_conn tests above exercise. Same env/tempdir setup shape
    /// as open_group_db_self_heals_a_never_created_file above.
    #[tokio::test]
    async fn create_and_get_document_round_trip_through_real_encrypted_group_db() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let persona_id = "real-path-persona";
        let group_id = "real-path-group";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        let verify = async {
            let doc_id = create_document(
                persona_id, group_id, key_hex, persona_id, "Real Doc", "hello",
            )
            .await
            .expect("create_document must succeed against a real encrypted group.db");

            let doc = get_document(persona_id, group_id, key_hex, &doc_id)
                .await
                .expect("get_document must succeed");
            assert_eq!(doc.content, "hello");
            assert_eq!(doc.title, "Real Doc");
            assert_eq!(doc.owner_persona_id, persona_id);
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    /// Regression coverage for the same bug class items.id=278 fixed for
    /// personal.db: open_group_db must self-heal a never-yet-created
    /// group.db by running migrate_group_db() itself, not hard-fail with
    /// SQLITE_CANTOPEN. Deliberately does NOT pre-create the file or call
    /// migrate_group_db() anywhere in this test -- that absence is exactly
    /// the fresh-(persona,group)-pair, first-access case under test.
    #[tokio::test]
    async fn open_group_db_self_heals_a_never_created_file() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let persona_id = "self-heal-persona";
        let group_id = "self-heal-group";
        let key_hex = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

        let db_path = get_group_db_path(persona_id, group_id);
        assert!(
            !db_path.exists(),
            "test setup must start with no db file -- that's the fresh-pair \
             first-access case under test"
        );

        let result = open_group_db(persona_id, group_id, key_hex).await;

        let verify = async {
            let mut conn = result.expect(
                "open_group_db must self-heal a fresh (persona, group) pair's \
                 never-created group.db, not hard-fail",
            );
            let row = sqlx::query("SELECT COUNT(*) AS n FROM documents")
                .fetch_one(&mut conn)
                .await
                .expect("documents table must exist after self-heal migration");
            let n: i64 = sqlx::Row::try_get(&row, "n").unwrap();
            assert_eq!(n, 0);
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }
}

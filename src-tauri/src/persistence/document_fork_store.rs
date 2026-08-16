// src-tauri/src/persistence/document_fork_store.rs
//
// items.id=286 (group.db 266d): canon-edit-or-fork -- the "fork to
// personal" half of GROUP_DB_DESIGN_20260802.md Section 2.3. The other
// half, "update canon," is group_store::update_document (items.id=285,
// already built).
//
// Operates on the `document_forks` table in personal.db (personal_004.sql)
// -- reuses personal_store::open_personal_db and PersonalStoreError rather
// than duplicating the opener, same reasoning as entity_store.rs's own
// header: a second opener would be a P4 (One Home) violation, since it's
// the same physical database file.
//
// CROSS-STORE BY NECESSITY: fork_document reads from group.db and writes to
// personal.db -- two different files, two different keys (account key vs.
// GroupKeyRegistry group key, design doc Section 2.1). The read side goes
// through group_store::get_document, the only public entry point into
// group.db's documents table (get_document_conn is module-private, not even
// pub(crate)) -- this is exactly the existing read-access check the task
// brief asks to reuse rather than reimplement: owner or any
// document_permissions row, per get_document's own doc comment.
//
// "VERSION" FIELD -- see personal_004.sql's own header for the full
// reasoning. Short version: documents has no version column, only
// updated_at, and it's the only thing update_document actually changes on a
// canon edit -- so it's what's copied into document_forks.source_canon_updated_at.
//
// CANON-VS-FORK "CHOICE" -- deliberately NOT built here. Presenting the
// user update-canon-or-fork as one decision is UX/command-layer work, left
// for whichever item builds the real UI/command layer -- matching the
// items.id=283/284/285 precedent of shipping library functions with no
// #[tauri::command] wrapper yet.
//
// QUERY STYLE: runtime sqlx::query() only -- no query!() macros (many-small-
// encrypted-DB topology, no static DATABASE_URL).

use thiserror::Error;

use crate::persistence::group_store::{self, GroupStoreError};
use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum DocumentForkError {
    #[error("Failed to read source document: {0}")]
    Source(#[from] GroupStoreError),
    #[error("Failed to write fork: {0}")]
    Fork(#[from] PersonalStoreError),
}

// ---------------------------------------------------------------------------
// Fork
// ---------------------------------------------------------------------------

/// Fork a group document into `persona_id`'s own personal.db: a clean-break,
/// provenance-tagged copy, independent from the moment it's created (design
/// doc Section 2.3). `persona_id` serves double duty, matching
/// group_store's own convention -- it is both whose local group.db copy is
/// read (with `group_store::get_document`'s existing owner-or-permission-row
/// check evaluated against this same persona_id) and whose personal.db the
/// fork is written into.
///
/// Read-access failure (including PermissionDenied for a persona with no
/// grant on the source document) surfaces as DocumentForkError::Source and
/// writes nothing -- the personal.db write only happens after the source
/// read succeeds.
///
/// Two separate key parameters because group.db and personal.db are
/// encrypted under different keys -- this is a plain library function (not
/// a #[tauri::command]); the future command-layer caller resolves both from
/// their respective registries, matching every existing store's key_hex
/// boundary convention.
#[allow(dead_code)] // items.id=286: ahead of its first real caller (no IPC layer yet, items.id=283/284/285 precedent)
pub async fn fork_document(
    user_id: &str,
    persona_id: &str,
    group_id: &str,
    group_key_hex: &str,
    personal_key_hex: &str,
    document_id: &str,
) -> Result<String, DocumentForkError> {
    let source =
        group_store::get_document(persona_id, group_id, group_key_hex, document_id).await?;

    let mut conn = open_personal_db(user_id, persona_id, personal_key_hex).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::providers::utils::now();

    sqlx::query(
        "INSERT INTO document_forks
            (id, title, content, source_group_id, source_document_id,
             source_canon_updated_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&source.title)
    .bind(&source.content)
    .bind(group_id)
    .bind(document_id)
    .bind(&source.updated_at)
    .bind(&now)
    .bind(&now)
    .execute(&mut conn)
    .await
    .map_err(|e| DocumentForkError::Fork(PersonalStoreError::Database(e)))?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    const GROUP_KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";
    const PERSONAL_KEY_HEX: &str = "aabbccddeeff00112233445566778899aabbccddeeff0011223344556677aa";

    struct ForkRow {
        title: String,
        content: String,
        source_group_id: String,
        source_document_id: String,
        source_canon_updated_at: String,
    }

    async fn fetch_fork_row(user_id: &str, persona_id: &str, fork_id: &str) -> Option<ForkRow> {
        let mut conn = open_personal_db(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .expect("open_personal_db failed");
        let row = sqlx::query(
            "SELECT title, content, source_group_id, source_document_id,
                    source_canon_updated_at
             FROM document_forks WHERE id = ?",
        )
        .bind(fork_id)
        .fetch_optional(&mut conn)
        .await
        .expect("query failed");
        row.map(|r| ForkRow {
            title: r.try_get("title").unwrap(),
            content: r.try_get("content").unwrap(),
            source_group_id: r.try_get("source_group_id").unwrap(),
            source_document_id: r.try_get("source_document_id").unwrap(),
            source_canon_updated_at: r.try_get("source_canon_updated_at").unwrap(),
        })
    }

    async fn count_document_forks(user_id: &str, persona_id: &str) -> i64 {
        let mut conn = open_personal_db(user_id, persona_id, PERSONAL_KEY_HEX)
            .await
            .expect("open_personal_db failed");
        let row = sqlx::query("SELECT COUNT(*) AS n FROM document_forks")
            .fetch_one(&mut conn)
            .await
            .expect("query failed");
        row.try_get("n").unwrap()
    }

    #[tokio::test]
    async fn fork_document_creates_an_independent_row_with_correct_provenance() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "fork-user";
        let persona_id = "fork-persona";
        let group_id = "fork-group";

        let verify = async {
            let doc_id = group_store::create_document(
                persona_id,
                group_id,
                GROUP_KEY_HEX,
                persona_id,
                "Family Budget",
                "original content",
            )
            .await
            .expect("create_document failed");

            let canon = group_store::get_document(persona_id, group_id, GROUP_KEY_HEX, &doc_id)
                .await
                .expect("get_document failed");

            let fork_id = fork_document(
                user_id,
                persona_id,
                group_id,
                GROUP_KEY_HEX,
                PERSONAL_KEY_HEX,
                &doc_id,
            )
            .await
            .expect("fork_document must succeed for a persona with read access");

            let row = fetch_fork_row(user_id, persona_id, &fork_id)
                .await
                .expect("forked row must exist in personal.db");

            assert_eq!(row.title, "Family Budget");
            assert_eq!(row.content, "original content");
            assert_eq!(row.source_group_id, group_id);
            assert_eq!(row.source_document_id, doc_id);
            assert_eq!(row.source_canon_updated_at, canon.updated_at);
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn fork_document_is_a_clean_break_not_a_live_link() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let user_id = "cleanbreak-user";
        let persona_id = "cleanbreak-persona";
        let group_id = "cleanbreak-group";

        let verify = async {
            let doc_id = group_store::create_document(
                persona_id,
                group_id,
                GROUP_KEY_HEX,
                persona_id,
                "Doc",
                "v1",
            )
            .await
            .expect("create_document failed");

            let fork_id = fork_document(
                user_id,
                persona_id,
                group_id,
                GROUP_KEY_HEX,
                PERSONAL_KEY_HEX,
                &doc_id,
            )
            .await
            .expect("fork_document failed");

            // Canon changes after the fork was taken.
            group_store::update_document(persona_id, group_id, GROUP_KEY_HEX, &doc_id, "v2")
                .await
                .expect("update_document failed");

            let row = fetch_fork_row(user_id, persona_id, &fork_id)
                .await
                .expect("forked row must still exist");
            assert_eq!(
                row.content, "v1",
                "a canon edit after fork must not change the already-forked copy"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }

    #[tokio::test]
    async fn fork_document_rejects_a_persona_with_no_read_access_and_writes_nothing() {
        let _lock = crate::test_support::ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let owner_persona_id = "denied-owner";
        let stranger_persona_id = "denied-stranger";
        let group_id = "denied-group";

        let verify = async {
            // group.db is one file PER (persona_id, group_id) pair -- each
            // member holds their own local copy (group_store.rs's own
            // header). To exercise the PermissionDenied path (as opposed to
            // DocumentNotFound), the document must exist in the stranger's
            // *own* local copy -- naming someone else as owner, exactly the
            // shape design doc Section 2.4's folder-sync would eventually
            // replicate into every member's local file. create_document
            // deliberately does not require persona_id == owner_persona_id
            // (see its own doc comment) -- used here to seed that state
            // directly, without waiting on folder-sync (items.id=287,
            // explicitly out of scope).
            let doc_id = group_store::create_document(
                stranger_persona_id,
                group_id,
                GROUP_KEY_HEX,
                owner_persona_id,
                "Policy Doc",
                "v1",
            )
            .await
            .expect("create_document failed");

            let result = fork_document(
                "denied-user",
                stranger_persona_id,
                group_id,
                GROUP_KEY_HEX,
                PERSONAL_KEY_HEX,
                &doc_id,
            )
            .await;

            assert!(
                matches!(
                    result,
                    Err(DocumentForkError::Source(
                        GroupStoreError::PermissionDenied(_, _)
                    ))
                ),
                "a persona with no read access must be rejected via the reused \
                 group_store read-access check: {result:?}"
            );

            let n = count_document_forks("denied-user", stranger_persona_id).await;
            assert_eq!(
                n, 0,
                "a rejected fork must not write any row to personal.db"
            );
        };
        verify.await;

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }
}

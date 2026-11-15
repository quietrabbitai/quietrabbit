// src-tauri/src/persistence/model_capability_store.rs
//
// READ side of shared.db's model_capabilities (items.id=896 stage 1,
// shared_045.sql). The only writers are the signed catalog apply
// (provider_catalog::apply) and shared_045.sql's seed, so this module has no
// insert, update or delete. The resolver (stage 2) reads the whole table once
// and works on the result.
//
// FAIL CLOSED PER ROW. A row whose closed-vocabulary text this build does not
// know (a task type, class, bar, subject or provenance added by a later
// catalog) is SKIPPED and counted, never guessed at: a skipped cell is an
// unmeasured cell, and an unmeasured cell is never chosen automatically.

use sqlx::{Row, SqliteConnection};

use crate::conductor::model_capability::{
    parse_vocab, AttainedBar, CapabilityRow, CapabilitySubject, Provenance,
};

/// The rows this build understands, and how many it could not read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilityTable {
    pub rows: Vec<CapabilityRow>,
    pub skipped: usize,
}

pub async fn list_capabilities(
    conn: &mut SqliteConnection,
) -> Result<CapabilityTable, sqlx::Error> {
    let raw = sqlx::query(
        "SELECT subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                model_class, sample_count, measured_at, provenance, note
           FROM model_capabilities
          ORDER BY subject_kind, provider_id, model_id, task_type, profile",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut table = CapabilityTable::default();
    for r in raw {
        let subject: String = r.try_get("subject_kind")?;
        let task: String = r.try_get("task_type")?;
        let bar: String = r.try_get("attained_bar")?;
        let class: String = r.try_get("model_class")?;
        let provenance: String = r.try_get("provenance")?;
        let samples: i64 = r.try_get("sample_count")?;
        let parsed = (
            parse_vocab::<CapabilitySubject>(&subject),
            parse_vocab(&task),
            parse_vocab::<AttainedBar>(&bar),
            parse_vocab(&class),
            parse_vocab::<Provenance>(&provenance),
            u32::try_from(samples).ok(),
        );
        let (Some(subject), Some(task_type), Some(attained_bar), Some(model_class)) =
            (parsed.0, parsed.1, parsed.2, parsed.3)
        else {
            table.skipped += 1;
            continue;
        };
        let (Some(provenance), Some(sample_count)) = (parsed.4, parsed.5) else {
            table.skipped += 1;
            continue;
        };
        table.rows.push(CapabilityRow {
            subject,
            provider_id: r.try_get("provider_id")?,
            model_id: r.try_get("model_id")?,
            task_type,
            profile: r.try_get("profile")?,
            attained_bar,
            model_class,
            sample_count,
            measured_at: r.try_get("measured_at")?,
            provenance,
            note: r.try_get("note")?,
        });
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::config::{ModelClass, ModelTaskType};
    use sqlx::SqlitePool;

    async fn migrated() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate");
        let pool = SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("pool");
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        (pool, dir)
    }

    #[tokio::test]
    async fn the_seeded_rows_all_parse_and_none_is_skipped() {
        let (pool, _dir) = migrated().await;
        let mut conn = pool.acquire().await.unwrap();
        let t = list_capabilities(&mut conn).await.unwrap();
        assert_eq!(t.skipped, 0);
        assert!(!t.rows.is_empty());
        for r in &t.rows {
            assert_eq!(r.subject, CapabilitySubject::Local, "{r:?}");
            assert!(r.provider_id.is_empty());
            assert_eq!(r.task_type, ModelTaskType::CreativeWriting);
            assert_eq!(r.provenance, Provenance::JasonRead);
            assert!(r.sample_count > 0);
        }
    }

    #[tokio::test]
    async fn a_row_with_a_value_this_build_does_not_know_is_skipped_not_guessed() {
        let (pool, _dir) = migrated().await;
        let mut conn = pool.acquire().await.unwrap();
        let before = list_capabilities(&mut conn).await.unwrap();
        sqlx::query(
            "INSERT INTO model_capabilities
               (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
                model_class, sample_count, measured_at, provenance)
             VALUES ('local', '', 'llama3.2:3b', 'poetry', '', 'basic', 'small', 1,
                     '2026-11-13', 'jason_read')",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        let after = list_capabilities(&mut conn).await.unwrap();
        assert_eq!(after.rows, before.rows);
        assert_eq!(after.skipped, 1);
        let _ = (ModelClass::Small,);
    }

    #[test]
    fn this_module_has_no_writer() {
        let src = include_str!("model_capability_store.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["INSERT", "UPDATE", "DELETE", "DROP", "ALTER"] {
            // The word may appear in the header comment, never in a statement.
            let in_code = body
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(forbidden));
            assert!(!in_code, "{forbidden} in the read-only store");
        }
    }
}

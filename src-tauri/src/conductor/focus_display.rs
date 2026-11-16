// src-tauri/src/conductor/focus_display.rs
//
// items.id=897 stage 1 (decisions.id=948 point 1): the ONE reader of what the
// generic Focus screen (items.id=902) shows about a Focus: its display name, its
// description and an optional one-line description.
//
// NOTHING IS COPIED. A Focus's display name and description already have one home:
// the `focuses` row in shared.db when the Focus has one (Writing Assistant), else
// the loaded .focus definition (the other five). load_focus_definition is the one
// loader that picks between them (DB row first), so this reader takes both fields
// from the FocusDefinition it returns. No new table holds a name or a description.
//
// WRITING ASSISTANT has its name and description in BOTH places (shared_038.sql
// and writing-assistant.focus). The loader reads the DB row for display and the
// file only for the tier cross-check, so the file's copy is never shown while the
// row exists. The two are kept (the DB columns are NOT NULL and the parser needs
// display_name, and the file is the fallback if the row is absent) and a drift test
// below pins that they agree. Removing either copy is a separate decision.
//
// ui_one_liner is the one NEW fact: at most 80 characters, placeholder wording that
// belongs to Chat-BRAND (items.id=506), because four of the six existing
// descriptions are longer than 100 characters. It lives in focus_launch_display and
// is None for a Focus that has no row.

use sqlx::SqlitePool;

use crate::conductor::launch_config::read_one_liner;
use crate::conductor::lifecycle::{load_focus_definition, LifecycleError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusDisplay {
    pub focus_id: String,
    pub display_name: String,
    pub description: String,
    pub ui_one_liner: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FocusDisplayError {
    #[error("{0}")]
    Load(#[from] LifecycleError),
    #[error("the display line could not be read: {0}")]
    Read(#[from] sqlx::Error),
}

/// What the screen shows about `focus_id`. An error loading the Focus is an error
/// here: a Focus that cannot be loaded cannot be launched either.
pub async fn read(pool: &SqlitePool, focus_id: &str) -> Result<FocusDisplay, FocusDisplayError> {
    let def = load_focus_definition(pool, focus_id).await?;
    let ui_one_liner = read_one_liner(pool, focus_id).await?;
    Ok(FocusDisplay {
        focus_id: def.focus_id,
        display_name: def.display_name,
        description: def.description,
        ui_one_liner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The text of a top-level `key:` in a .focus file, folded scalars joined with
    /// single spaces.
    fn file_field(text: &str, key: &str) -> String {
        let mut lines = text.lines();
        let prefix = format!("{key}:");
        while let Some(l) = lines.next() {
            let Some(rest) = l.strip_prefix(&prefix) else {
                continue;
            };
            let rest = rest.trim();
            if !rest.is_empty() && rest != ">" && rest != "|" {
                return rest.trim_matches('"').to_owned();
            }
            let mut parts = Vec::new();
            for l in lines.by_ref() {
                if l.starts_with(' ') && !l.trim().is_empty() {
                    parts.push(l.trim().to_owned());
                } else {
                    break;
                }
            }
            return parts.join(" ");
        }
        panic!("no top-level {key}: in the file");
    }

    fn wa_file() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("app")
            .join("core_artifacts")
            .join("focuses")
            .join("writing-assistant.focus");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[tokio::test]
    async fn writing_assistants_two_copies_of_its_name_and_description_agree() {
        // decisions.id=948 point 1: the name stays in both places (the shared.db row
        // and the .focus file), pinned against drift. No third copy exists.
        let (pool, _dir) = migrated().await;
        let row: (String, String) = sqlx::query_as(
            "SELECT display_name, description FROM focuses WHERE focus_id = 'writing-assistant'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let file = wa_file();
        assert_eq!(row.0.trim(), file_field(&file, "display_name"));
        assert_eq!(row.1.trim(), file_field(&file, "description"));
    }

    #[tokio::test]
    async fn the_reader_supplies_name_and_description_from_the_loader_for_a_db_focus() {
        let (pool, _dir) = migrated().await;
        let d = read(&pool, "writing-assistant").await.unwrap();
        assert_eq!(d.focus_id, "writing-assistant");
        assert_eq!(d.display_name, "Writing Assistant");
        assert!(!d.description.trim().is_empty());
        assert!(d.ui_one_liner.is_some());
        let row: String = sqlx::query_scalar(
            "SELECT display_name FROM focuses WHERE focus_id = 'writing-assistant'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(d.display_name, row);
    }

    #[test]
    fn no_launch_table_has_a_name_or_description_column() {
        // The new tables hold no copy of a display name or a description.
        use crate::persistence::shared_db_residual::RESIDUAL_SHARED_SCHEMA;
        for t in RESIDUAL_SHARED_SCHEMA
            .iter()
            .filter(|t| t.name.starts_with("focus_launch_") || t.name == "focus_mode_profiles")
        {
            for c in t.columns {
                assert!(
                    !matches!(*c, "display_name" | "description" | "name"),
                    "{}.{c} would copy a display fact",
                    t.name
                );
            }
        }
    }
}

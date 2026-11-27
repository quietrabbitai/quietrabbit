// src-tauri/src/commands/my_facts.rs
//
// items.id=959 unit 1 (decisions.id=968, 969): the session-guarded READ commands
// behind the Library Facts view. GATE-CRITICAL: the failure direction is a read of
// another Persona's facts.
//
// Every command takes one persona_id, takes the writer gate first (the
// device_library and shared_log pattern), derives the user and key from the session
// (never from an argument), and checks the user belongs to that Persona before the
// Persona's personal.db is opened. No command takes a second Persona and none lists
// across Personas (decisions.id=546, 968). Errors are short codes: persona_not_found,
// facts_unreadable (fails closed, no partial tree), fact_not_found, query_empty.
//
// Unit 2 adds the direct, non-model writes: stop_using_fact, stop_using_section,
// use_again_fact, use_again_section and the read list_removed. They take the same gate,
// session and membership check, and removal is end-dating only (no DELETE anywhere).
// Add and change are NOT here (items.id=961).

use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::persistence::facts_stop_store as stop;
use crate::persistence::facts_view_store::{
    self as store, EarlierValue, FactsTree, FactsViewError, RemovedList,
};
use crate::persistence::personal_store::open_personal_db;

fn code(e: FactsViewError) -> String {
    e.to_string()
}

async fn open_checked(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
) -> Result<sqlx::SqliteConnection, String> {
    crate::commands::collection_consent::require_persona(pool, user_id, persona_id).await?;
    open_personal_db(user_id, persona_id, key_hex)
        .await
        .map_err(|e| {
            log::warn!("my_facts: could not open the Persona's file: {e}");
            code(FactsViewError::Unreadable)
        })
}

pub(crate) async fn facts_tree_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
) -> Result<FactsTree, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    store::facts_tree_conn(&mut conn, persona_id)
        .await
        .map_err(code)
}

pub(crate) async fn fact_history_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    fact_id: &str,
) -> Result<Vec<EarlierValue>, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    store::fact_history_conn(&mut conn, fact_id)
        .await
        .map_err(code)
}

pub(crate) async fn search_facts_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    query: &str,
) -> Result<FactsTree, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    store::search_facts_conn(&mut conn, persona_id, query)
        .await
        .map_err(code)
}

pub(crate) async fn list_removed_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
) -> Result<RemovedList, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    store::list_removed_conn(&mut conn).await.map_err(code)
}

pub(crate) async fn stop_using_fact_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    fact_id: &str,
) -> Result<(), String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    stop::stop_fact_conn(&mut conn, fact_id).await.map_err(code)
}

pub(crate) async fn stop_using_section_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    section_id: &str,
) -> Result<u32, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    stop::stop_section_conn(&mut conn, section_id)
        .await
        .map_err(code)
}

pub(crate) async fn use_again_fact_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    fact_id: &str,
) -> Result<(), String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    stop::use_again_fact_conn(&mut conn, fact_id)
        .await
        .map_err(code)
}

pub(crate) async fn use_again_section_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    section_id: &str,
) -> Result<u32, String> {
    let mut conn = open_checked(pool, user_id, key_hex, persona_id).await?;
    stop::use_again_section_conn(&mut conn, section_id)
        .await
        .map_err(code)
}

#[tauri::command]
#[specta::specta]
pub async fn get_facts_tree(
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FactsTree, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    facts_tree_for_session(&pool, &user_id, &key_hex, &persona_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_fact_history(
    persona_id: String,
    fact_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<EarlierValue>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    fact_history_for_session(&pool, &user_id, &key_hex, &persona_id, &fact_id).await
}

#[tauri::command]
#[specta::specta]
pub async fn search_facts(
    persona_id: String,
    query: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<FactsTree, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    search_facts_for_session(&pool, &user_id, &key_hex, &persona_id, &query).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_removed(
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<RemovedList, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    list_removed_for_session(&pool, &user_id, &key_hex, &persona_id).await
}

/// Stop using one fact. Direct, no model, no reason asked (decisions.id=968).
#[tauri::command]
#[specta::specta]
pub async fn stop_using_fact(
    persona_id: String,
    fact_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    stop_using_fact_for_session(&pool, &user_id, &key_hex, &persona_id, &fact_id).await
}

/// Stop using a section. Returns how many of its own facts were ended.
#[tauri::command]
#[specta::specta]
pub async fn stop_using_section(
    persona_id: String,
    section_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<u32, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    stop_using_section_for_session(&pool, &user_id, &key_hex, &persona_id, &section_id).await
}

/// Use a stopped fact again (a new active row). `fact_id` is RemovedFact.id.
#[tauri::command]
#[specta::specta]
pub async fn use_again_fact(
    persona_id: String,
    fact_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    use_again_fact_for_session(&pool, &user_id, &key_hex, &persona_id, &fact_id).await
}

/// Use a stopped section again. Returns how many facts came back.
#[tauri::command]
#[specta::specta]
pub async fn use_again_section(
    persona_id: String,
    section_id: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<u32, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    use_again_section_for_session(&pool, &user_id, &key_hex, &persona_id, &section_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::registry::key_hex;
    use crate::persistence::entity_store;
    use crate::persistence::persona_store;
    use crate::persistence::personal_store::create_entity_fact_with_provenance_conn;
    use crate::test_support::{mock_app_with_registry, ENV_MUTEX};
    use tauri::Manager;

    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xCDu8; crate::auth::kdf::MASTER_KEY_LEN];
    const OTHER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0x11u8; crate::auth::kdf::MASTER_KEY_LEN];

    struct Env {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
        user: String,
        /// Two Personas of the SAME user, and one Persona of another user.
        a: String,
        b: String,
        stranger_persona: String,
        key: String,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn add_user(pool: &sqlx::SqlitePool, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            pool,
            &id,
            name,
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user");
        id
    }

    async fn add_persona(pool: &sqlx::SqlitePool, user: &str, key: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(pool, &id, "P", "personal", user, key, None)
            .await
            .expect("create_persona");
        id
    }

    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        let key = key_hex(&MASTER_KEY);
        let user = add_user(&pool, "Facts User").await;
        let a = add_persona(&pool, &user, &key).await;
        let b = add_persona(&pool, &user, &key).await;
        let stranger = add_user(&pool, "Someone Else").await;
        let stranger_persona = add_persona(&pool, &stranger, &key).await;
        Env {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
            user,
            a,
            b,
            stranger_persona,
            key,
        }
    }

    async fn conn(e: &Env, persona: &str) -> sqlx::SqliteConnection {
        open_personal_db(&e.user, persona, &e.key).await.unwrap()
    }

    async fn fact(
        c: &mut sqlx::SqliteConnection,
        persona: &str,
        entity: Option<&str>,
        label: &str,
        value: &str,
        source: &str,
    ) -> String {
        create_entity_fact_with_provenance_conn(
            c, entity, label, value, "personal", source, persona, false, None, None,
        )
        .await
        .unwrap()
    }

    async fn entity(
        c: &mut sqlx::SqliteConnection,
        name: &str,
        aliases: &[&str],
        parent: Option<&str>,
    ) -> String {
        let aliases: Vec<String> = aliases.iter().map(|s| (*s).to_owned()).collect();
        entity_store::create_entity_conn(c, "person", name, &aliases, parent, None)
            .await
            .unwrap()
    }

    async fn owned(c: &mut sqlx::SqliteConnection, name: &str) -> String {
        entity_store::create_owned_entity_conn(c, "tech-support", "focus_subject", name, &[], None)
            .await
            .unwrap()
    }

    async fn tree(e: &Env, persona: &str) -> Result<FactsTree, String> {
        facts_tree_for_session(&e.pool, &e.user, &e.key, persona).await
    }

    fn names(t: &FactsTree) -> Vec<&str> {
        t.sections.iter().map(|s| s.name.as_str()).collect()
    }

    fn labels(t: &FactsTree) -> Vec<String> {
        let mut v: Vec<String> = t.general.iter().map(|f| f.label.clone()).collect();
        for s in &t.sections {
            v.extend(s.facts.iter().map(|f| format!("{}/{}", s.name, f.label)));
        }
        v
    }

    /// items.id=960 unit 1: copied_in is "the source Persona is not this one", in the
    /// tree and in search, whatever cross_persona_export says, and it agrees with
    /// what the Gate 1 bridge does with the same rows: under a stored Share the
    /// bridge releases exactly the General facts that are not copied in.
    #[tokio::test]
    async fn copied_in_is_the_bridges_own_cross_persona_test() {
        use crate::conductor::personal_fact_fields::registry;
        use crate::persistence::fact_policy_store::{FactPolicyRow, UserPolicies};
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        // (label, source is this Persona, marked as an export)
        let rows = [
            ("location_city", true, false),
            ("job_title", true, true),
            ("employer", false, true),
            ("industry", false, false),
        ];
        for (label, own, export) in rows {
            let source = if own { &e.a } else { &e.b };
            create_entity_fact_with_provenance_conn(
                &mut c,
                None,
                label,
                "Zanzibar",
                "personal",
                "user_confirmed",
                source,
                export,
                export.then_some(e.b.as_str()),
                None,
            )
            .await
            .unwrap();
        }
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "own_note", "Zanzibar", "ipc").await;
        create_entity_fact_with_provenance_conn(
            &mut c,
            Some(&maya),
            "copied_note",
            "Zanzibar",
            "personal",
            "user_confirmed",
            &e.b,
            true,
            Some(&e.b),
            None,
        )
        .await
        .unwrap();
        drop(c);

        let expected = [
            ("copied_note", true),
            ("employer", true),
            ("industry", true),
            ("job_title", false),
            ("location_city", false),
            ("own_note", false),
        ];
        let flags = |t: &FactsTree| {
            let mut v: Vec<(String, bool)> = t
                .general
                .iter()
                .chain(t.sections.iter().flat_map(|s| s.facts.iter()))
                .map(|f| (f.label.clone(), f.copied_in))
                .collect();
            v.sort();
            v
        };
        let expected: Vec<(String, bool)> = expected
            .iter()
            .map(|(l, c)| ((*l).to_owned(), *c))
            .collect();
        assert_eq!(flags(&tree(&e, &e.a).await.unwrap()), expected);
        // Search by value, and by section name (the second read of search).
        let by_value = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, "zanzibar")
            .await
            .unwrap();
        assert_eq!(flags(&by_value), expected);
        let by_name = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, "maya")
            .await
            .unwrap();
        assert_eq!(
            flags(&by_name),
            [
                ("copied_note".to_owned(), true),
                ("own_note".to_owned(), false)
            ]
        );

        // The bridge over the same file, every field at Share.
        let policies = UserPolicies::Rows(
            rows.iter()
                .map(|(label, ..)| FactPolicyRow {
                    field_name: (*label).to_owned(),
                    tier2: Some("pass".to_owned()),
                    tier3: None,
                    set_at: String::new(),
                    set_by: "user".to_owned(),
                })
                .collect(),
        );
        let loaded = crate::persistence::personal_store::load_entity_facts_for_context(
            &e.user, &e.a, &e.key,
        )
        .await
        .unwrap();
        let bridged = crate::conductor::personal_fact_bridge::bridge_fields(
            loaded.iter(),
            &e.a,
            registry().map_err(|_| ()),
            &policies,
        );
        assert_eq!(bridged.len(), rows.len());
        for field in bridged {
            let copied = expected
                .iter()
                .find(|(l, _)| *l == field.field_name)
                .unwrap()
                .1;
            assert_eq!(
                field.abstraction_tier2,
                if copied { "not_permitted" } else { "pass" },
                "{}",
                field.field_name
            );
        }
    }

    /// A row with no source Persona (the insert trigger forbids one; this drops it)
    /// counts as copied in.
    #[tokio::test]
    async fn a_fact_with_no_source_persona_counts_as_copied_in() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        sqlx::query("DROP TRIGGER trg_entity_facts_provenance_required")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO entity_facts
             (id, entity_id, field_name, field_value, sensitivity, source, created_at)
             VALUES ('no-source', NULL, 'employer', 'Acme', 'personal', 'ipc', 't')",
        )
        .execute(&mut c)
        .await
        .unwrap();
        drop(c);
        assert!(tree(&e, &e.a).await.unwrap().general[0].copied_in);
    }

    /// General then sections A to Z; provenance and earlier counts come through.
    #[tokio::test]
    async fn the_tree_is_general_then_sections_with_provenance() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        fact(&mut c, &e.a, None, "location_city", "Leeds", "ipc").await;
        fact(&mut c, &e.a, None, "job_title", "Nurse", "interview").await;
        let zed = entity(&mut c, "Zed", &[], None).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "corrected").await;
        fact(
            &mut c,
            &e.a,
            Some(&maya),
            "City",
            "Bristol",
            "user_confirmed",
        )
        .await;
        fact(&mut c, &e.a, Some(&zed), "Vet", "Parkside", "conversation").await;
        drop(c);

        let t = tree(&e, &e.a).await.unwrap();
        assert_eq!(names(&t), ["Maya", "Zed"]);
        assert_eq!(t.fact_count, 4);
        let city = &t.sections[0].facts[0];
        assert_eq!(city.value, "Bristol");
        assert_eq!(city.earlier_count, 1);
        assert_eq!(city.provenance, Some(store::FactProvenance::YouConfirmed));
        let by: std::collections::HashMap<_, _> = t
            .general
            .iter()
            .map(|f| (f.label.as_str(), f.provenance))
            .collect();
        assert_eq!(
            by["location_city"],
            Some(store::FactProvenance::YouConfirmed)
        );
        // interview is the column default: no provenance line, never an invented one.
        assert_eq!(by["job_title"], None);
        assert_eq!(
            t.sections[1].facts[0].provenance,
            Some(store::FactProvenance::FromChat)
        );
    }

    /// Only a General fact written by a Persona sync warns that a stop may not last.
    #[tokio::test]
    async fn only_a_synced_general_fact_warns_that_a_stop_may_not_last() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        fact(&mut c, &e.a, None, "employer", "Acme", "synced_share").await;
        fact(&mut c, &e.a, None, "industry", "Retail", "ipc").await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "synced_share").await;
        drop(c);
        let t = tree(&e, &e.a).await.unwrap();
        let flagged: Vec<_> = t
            .general
            .iter()
            .chain(t.sections.iter().flat_map(|s| s.facts.iter()))
            .filter(|f| f.sync_may_restore)
            .map(|f| f.label.as_str())
            .collect();
        assert_eq!(flagged, ["employer"]);
    }

    /// The mapping is closed: every other source value shows no line.
    #[test]
    fn provenance_maps_only_the_listed_sources() {
        use store::{provenance_of, FactProvenance as P};
        assert_eq!(provenance_of("user_confirmed"), Some(P::YouConfirmed));
        assert_eq!(provenance_of("ipc"), Some(P::YouConfirmed));
        assert_eq!(provenance_of("extract_confirm"), Some(P::YouConfirmed));
        assert_eq!(provenance_of("corrected"), Some(P::YouCorrected));
        assert_eq!(provenance_of("conversation"), Some(P::FromChat));
        assert_eq!(provenance_of("extracted"), Some(P::FoundByQuietRabbit));
        for other in ["interview", "synced_share", "test", "", "IPC"] {
            assert_eq!(provenance_of(other), None, "{other}");
        }
    }

    /// Owned (device) entities and their facts never appear: tree, search, history.
    #[tokio::test]
    async fn an_owned_entity_never_appears_anywhere() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        let dev = owned(&mut c, "Netgear Router").await;
        let dev_fact = fact(&mut c, &e.a, Some(&dev), "firmware", "SECRETFW-9", "ipc").await;
        // A child of an owned entity is not a way around the filter either.
        let kid = entity(&mut c, "Kid Of Device", &[], Some(&dev)).await;
        fact(&mut c, &e.a, Some(&kid), "note", "SECRETKID-1", "ipc").await;
        drop(c);

        let t = tree(&e, &e.a).await.unwrap();
        assert_eq!(names(&t), ["Maya"]);
        assert_eq!(t.fact_count, 1);
        for q in ["netgear", "SECRETFW", "SECRETKID", "firmware", "Kid Of"] {
            let s = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, q)
                .await
                .unwrap();
            assert!(s.sections.is_empty() && s.general.is_empty(), "{q}");
        }
        let h = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, &dev_fact).await;
        assert_eq!(h.unwrap_err(), "fact_not_found");
    }

    /// A stopped parent hides its child at read time; the child row is untouched.
    #[tokio::test]
    async fn a_stopped_parent_hides_its_children_at_read_time() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        fact(&mut c, &e.a, Some(&kid), "Size", "Small", "ipc").await;
        let t = {
            drop(c);
            tree(&e, &e.a).await.unwrap()
        };
        assert_eq!(names(&t), ["Home", "Garden"]);
        assert_eq!(t.sections[1].parent_id.as_deref(), Some(home.as_str()));

        let mut c = conn(&e, &e.a).await;
        entity_store::retire_entity_conn(&mut c, &home, "archived")
            .await
            .unwrap();
        drop(c);
        let t = tree(&e, &e.a).await.unwrap();
        assert!(
            t.sections.is_empty(),
            "the child follows the stopped parent"
        );
        assert_eq!(t.fact_count, 0);
    }

    /// Earlier values come back newest first; an end-dated fact id is not a fact.
    #[tokio::test]
    async fn history_lists_earlier_values_for_a_visible_fact_only() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        let old = fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        let new = fact(&mut c, &e.a, Some(&maya), "City", "Bristol", "ipc").await;
        drop(c);
        let h = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, &new)
            .await
            .unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].value, "Leeds");
        // The superseded row is history, not an addressable fact.
        let r = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, &old).await;
        assert_eq!(r.unwrap_err(), "fact_not_found");
        let r = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, "no-such").await;
        assert_eq!(r.unwrap_err(), "fact_not_found");
    }

    /// Search covers names, aliases (json_each), labels and values, this Persona only.
    #[tokio::test]
    async fn search_matches_names_aliases_labels_and_values() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        fact(&mut c, &e.a, None, "employer", "Acme Ltd", "ipc").await;
        fact(&mut c, &e.a, None, "home_town", "York", "ipc").await;
        let maya = entity(&mut c, "Maya", &["Mimi", "100%_sis"], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        fact(&mut c, &e.a, Some(&maya), "Birthday", "14 July", "ipc").await;
        let max = entity(&mut c, "Max", &[], None).await;
        fact(&mut c, &e.a, Some(&max), "Vet", "Parkside", "ipc").await;
        drop(c);
        let s = |q: &'static str| {
            let (p, u, k, a) = (e.pool.clone(), e.user.clone(), e.key.clone(), e.a.clone());
            async move { search_facts_for_session(&p, &u, &k, &a, q).await.unwrap() }
        };

        // A value: only the matching fact.
        assert_eq!(labels(&s("leeds").await), ["Maya/City"]);
        // A label.
        assert_eq!(labels(&s("birthday").await), ["Maya/Birthday"]);
        // A name: the whole section.
        assert_eq!(labels(&s("maya").await), ["Maya/Birthday", "Maya/City"]);
        // An alias, and LIKE metacharacters are literal.
        assert_eq!(labels(&s("mimi").await), ["Maya/Birthday", "Maya/City"]);
        assert_eq!(labels(&s("100%_").await), ["Maya/Birthday", "Maya/City"]);
        assert!(
            s("%").await.sections.len() == 1,
            "only the alias with a percent sign"
        );
        // A General fact.
        assert_eq!(labels(&s("acme").await), ["employer"]);
        // A stored field name also matches in its spaced form.
        assert_eq!(labels(&s("home town").await), ["home_town"]);
        assert_eq!(s("acme").await.fact_count, 1);
        // No match.
        let none = s("zzz").await;
        assert!(none.general.is_empty() && none.sections.is_empty());
        let r = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, "   ").await;
        assert_eq!(r.unwrap_err(), "query_empty");
    }

    /// A matching child keeps its parent in the result so it stays reachable.
    #[tokio::test]
    async fn a_matching_child_brings_its_ancestors_without_their_facts() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        fact(&mut c, &e.a, Some(&home), "Address", "1 High St", "ipc").await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        fact(&mut c, &e.a, Some(&kid), "Size", "Small", "ipc").await;
        drop(c);
        let s = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, "small")
            .await
            .unwrap();
        assert_eq!(names(&s), ["Home", "Garden"]);
        assert!(s.sections[0].facts.is_empty());
        assert_eq!(s.fact_count, 1);
    }

    /// ISOLATION (decisions.id=546): Persona B's facts, entities and ids are
    /// unreachable through Persona A, by tree, by search and by fact id.
    #[tokio::test]
    async fn another_personas_facts_are_unreachable_by_command_and_by_search() {
        let e = env().await;
        let mut ca = conn(&e, &e.a).await;
        fact(&mut ca, &e.a, None, "location_city", "Leeds", "ipc").await;
        drop(ca);
        let mut cb = conn(&e, &e.b).await;
        fact(
            &mut cb,
            &e.b,
            None,
            "location_city",
            "BRISTOLONLYINB",
            "ipc",
        )
        .await;
        let zoe = entity(&mut cb, "ZoeOnlyInB", &["ZoeAliasB"], None).await;
        let b_fact = fact(&mut cb, &e.b, Some(&zoe), "Vet", "VETONLYINB", "ipc").await;
        drop(cb);

        let ta = tree(&e, &e.a).await.unwrap();
        assert_eq!(labels(&ta), ["location_city"]);
        assert_eq!(ta.general[0].value, "Leeds");
        assert!(ta.sections.is_empty());
        for q in [
            "BRISTOLONLYINB",
            "ZoeOnlyInB",
            "ZoeAliasB",
            "VETONLYINB",
            "vet",
        ] {
            let s = search_facts_for_session(&e.pool, &e.user, &e.key, &e.a, q)
                .await
                .unwrap();
            assert!(s.sections.is_empty(), "{q} reached another Persona");
            assert!(
                s.general.iter().all(|f| f.value == "Leeds"),
                "{q} reached another Persona"
            );
        }
        // B's own view still has it, so the emptiness above is isolation, not absence.
        let tb = tree(&e, &e.b).await.unwrap();
        assert_eq!(names(&tb), ["ZoeOnlyInB"]);
        // B's fact id, asked through A, resolves to nothing.
        let r = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, &b_fact).await;
        assert_eq!(r.unwrap_err(), "fact_not_found");
        // Asked through B it does resolve.
        assert!(
            fact_history_for_session(&e.pool, &e.user, &e.key, &e.b, &b_fact)
                .await
                .is_ok()
        );
    }

    /// A Persona the session user does not belong to, a malformed id and a missing
    /// one all answer the same code, and read nothing.
    #[tokio::test]
    async fn a_persona_the_user_is_not_in_is_not_found() {
        let e = env().await;
        let mut c = conn(&e, &e.stranger_persona).await;
        fact(
            &mut c,
            &e.stranger_persona,
            None,
            "location_city",
            "Elsewhere",
            "ipc",
        )
        .await;
        drop(c);
        for p in [
            e.stranger_persona.as_str(),
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert_eq!(tree(&e, p).await.unwrap_err(), "persona_not_found", "{p}");
            let r = search_facts_for_session(&e.pool, &e.user, &e.key, p, "elsewhere").await;
            assert_eq!(r.unwrap_err(), "persona_not_found", "{p}");
            let r = fact_history_for_session(&e.pool, &e.user, &e.key, p, "x").await;
            assert_eq!(r.unwrap_err(), "persona_not_found", "{p}");
        }
    }

    /// FAILS CLOSED: a Persona file that cannot be read (wrong key) gives one code and
    /// no partial tree, for every command.
    #[tokio::test]
    async fn an_unreadable_persona_file_fails_closed() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        fact(&mut c, &e.a, None, "location_city", "Leeds", "ipc").await;
        drop(c);
        let bad = key_hex(&OTHER_KEY);
        let r = facts_tree_for_session(&e.pool, &e.user, &bad, &e.a).await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = search_facts_for_session(&e.pool, &e.user, &bad, &e.a, "leeds").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = fact_history_for_session(&e.pool, &e.user, &bad, &e.a, "x").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
    }

    /// A malformed row fails the whole read, not just the row.
    #[tokio::test]
    async fn a_malformed_entity_row_fails_the_whole_read() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        entity(&mut c, "Fine", &[], None).await;
        // aliases carries a json_valid CHECK, so corrupt it to valid-but-wrong JSON.
        sqlx::query("UPDATE entities SET aliases = '{\"a\":1}' WHERE id = ?")
            .bind(&maya)
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);
        assert_eq!(tree(&e, &e.a).await.unwrap_err(), "facts_unreadable");
    }

    /// The commands need a session, and use the session's own user.
    #[tokio::test]
    async fn the_commands_need_a_session() {
        let e = env().await;
        let app = mock_app_with_registry(e.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();
        assert!(get_facts_tree(e.a.clone(), registry.clone(), pool.clone())
            .await
            .is_err());
        assert!(
            get_fact_history(e.a.clone(), "x".into(), registry.clone(), pool.clone())
                .await
                .is_err()
        );
        assert!(
            search_facts(e.a.clone(), "x".into(), registry.clone(), pool.clone())
                .await
                .is_err()
        );
    }

    fn production(src: &'static str) -> &'static str {
        src.split("#[cfg(test)]").next().unwrap()
    }

    /// Each command takes the gate first, derives the user from the session, takes no
    /// user id, key, AppHandle or second Persona, and has a _for_session body.
    #[test]
    fn every_command_takes_the_gate_first_and_one_persona() {
        let src = production(include_str!("my_facts.rs"));
        let commands: Vec<&str> = src.split("#[tauri::command]").skip(1).collect();
        assert_eq!(
            commands.len(),
            8,
            "the read set and the unit 2 writes, no more"
        );
        for body in commands {
            let sig_end = body.find("{\n").unwrap();
            let sig = &body[..sig_end];
            assert!(!sig.contains("user_id"), "no user_id argument: {sig}");
            assert!(!sig.contains("key_hex"), "no key argument: {sig}");
            assert!(!sig.contains("AppHandle"), "{sig}");
            assert_eq!(sig.matches("persona_id").count(), 1, "one Persona: {sig}");
            let after = &body[sig_end..];
            let gate = after
                .find("writer_gate::command(&key_registry)")
                .expect("gate");
            let session = after
                .find("session_user_and_key(&key_registry)")
                .expect("session");
            assert!(gate < session, "the gate comes first");
            assert!(after.contains("_for_session("), "a _for_session body");
        }
        // The membership check precedes the open in the one place that opens a file.
        let open = src.find("async fn open_checked").unwrap();
        let body = &src[open..];
        assert!(
            body.find("require_persona").unwrap() < body.find("open_personal_db").unwrap(),
            "the membership check comes before the file is opened"
        );
    }

    /// The store is read-only and owner-filtered: no write statement, no DELETE, and
    /// every SELECT on entities names `owner_focus IS NULL`.
    #[test]
    fn the_store_is_read_only_and_every_entity_read_is_owner_filtered() {
        // Code only: the header comments name what is not used.
        let code: String = production(include_str!("../persistence/facts_view_store.rs"))
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = code.as_str();
        for bad in ["INSERT ", "UPDATE ", "DELETE ", "DROP ", "ALTER "] {
            assert!(!src.contains(bad), "a write statement: {bad}");
        }
        let mut reads = 0;
        for part in src.split("FROM entities").skip(1) {
            let window: String = part.chars().take(160).collect();
            assert!(
                window.contains("owner_focus IS NULL"),
                "an entities read without the owner filter: {window}"
            );
            reads += 1;
        }
        assert_eq!(
            reads, 4,
            "the section loader, the stop counts, the alias search and the removed sections"
        );
        assert!(!src.contains("entity_store::"), "no entity_store reader");
        assert!(
            !src.contains("search_entities"),
            "search_entities not widened"
        );
    }

    // ------------------------------------------------------------------
    // Unit 2: stop using, Removed, use again (items.id=959, decisions.id=969)
    // ------------------------------------------------------------------

    async fn meta(c: &mut sqlx::SqliteConnection, id: &str) -> serde_json::Value {
        let s: String = sqlx::query_scalar("SELECT extra_metadata FROM entity_facts WHERE id = ?")
            .bind(id)
            .fetch_one(c)
            .await
            .unwrap();
        serde_json::from_str(&s).unwrap()
    }

    async fn count_facts(c: &mut sqlx::SqliteConnection) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM entity_facts")
            .fetch_one(c)
            .await
            .unwrap()
    }

    async fn removed(e: &Env, persona: &str) -> RemovedList {
        list_removed_for_session(&e.pool, &e.user, &e.key, persona)
            .await
            .unwrap()
    }

    async fn stop_fact(e: &Env, persona: &str, id: &str) -> Result<(), String> {
        stop_using_fact_for_session(&e.pool, &e.user, &e.key, persona, id).await
    }

    async fn stop_section(e: &Env, persona: &str, id: &str) -> Result<u32, String> {
        stop_using_section_for_session(&e.pool, &e.user, &e.key, persona, id).await
    }

    async fn again_fact(e: &Env, persona: &str, id: &str) -> Result<(), String> {
        use_again_fact_for_session(&e.pool, &e.user, &e.key, persona, id).await
    }

    async fn again_section(e: &Env, persona: &str, id: &str) -> Result<u32, String> {
        use_again_section_for_session(&e.pool, &e.user, &e.key, persona, id).await
    }

    /// Stopping a fact end-dates it with a marker, leaves the row, and lists it.
    /// A stop is not a supersession: it is not an earlier value.
    #[tokio::test]
    async fn stop_using_a_fact_end_dates_marks_and_lists_it() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        let old = fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        let city = fact(&mut c, &e.a, Some(&maya), "City", "Bristol", "ipc").await;
        let job = fact(&mut c, &e.a, None, "job_title", "Nurse", "ipc").await;
        let before = count_facts(&mut c).await;
        // A pristine (imported, unedited) record flips to user_modified on a stop.
        sqlx::query("UPDATE entities SET modification_state = 'pristine' WHERE id = ?")
            .bind(&maya)
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);

        stop_fact(&e, &e.a, &city).await.unwrap();
        stop_fact(&e, &e.a, &job).await.unwrap();

        let t = tree(&e, &e.a).await.unwrap();
        assert!(t.general.is_empty());
        assert_eq!(t.fact_count, 0);
        let r = removed(&e, &e.a).await;
        assert_eq!(r.count, 2);
        assert!(r.sections.is_empty());
        let got = r.facts.iter().find(|f| f.label == "City").unwrap();
        assert_eq!(got.id, city);
        assert_eq!(got.value, "Bristol");
        assert_eq!(got.section_name.as_deref(), Some("Maya"));
        assert!(r
            .facts
            .iter()
            .any(|f| f.id == job && f.section_name.is_none()));

        let mut c = conn(&e, &e.a).await;
        assert_eq!(count_facts(&mut c).await, before, "nothing was deleted");
        let m = meta(&mut c, &city).await;
        assert_eq!(m["stop_scope"], "fact");
        let until: String = sqlx::query_scalar("SELECT valid_until FROM entity_facts WHERE id = ?")
            .bind(&city)
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(m["stopped_at"], until.as_str());
        // The earlier superseded row has no marker.
        assert!(meta(&mut c, &old).await.get("stopped_at").is_none());
        // The entity was edited by the user.
        let state: String =
            sqlx::query_scalar("SELECT modification_state FROM entities WHERE id = ?")
                .bind(&maya)
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert_eq!(state, "user_modified");
    }

    /// A stopped row is removed, never an earlier value: not in earlier_count, not in
    /// history, even once the value is used again.
    #[tokio::test]
    async fn a_stopped_row_is_never_an_earlier_value() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        let city = fact(&mut c, &e.a, Some(&maya), "City", "Bristol", "ipc").await;
        drop(c);
        let hist = |id: String| {
            let (p, u, k, a) = (e.pool.clone(), e.user.clone(), e.key.clone(), e.a.clone());
            async move { fact_history_for_session(&p, &u, &k, &a, &id).await.unwrap() }
        };
        assert_eq!(
            tree(&e, &e.a).await.unwrap().sections[0].facts[0].earlier_count,
            1
        );

        stop_fact(&e, &e.a, &city).await.unwrap();
        again_fact(&e, &e.a, &city).await.unwrap();

        let t = tree(&e, &e.a).await.unwrap();
        let back = &t.sections[0].facts[0];
        assert_ne!(back.id, city, "a new row, not the stopped one");
        assert_eq!(back.value, "Bristol");
        assert_eq!(
            back.earlier_count, 1,
            "Leeds only: Bristol is not its own history"
        );
        let h = hist(back.id.clone()).await;
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].value, "Leeds");
        assert_eq!(removed(&e, &e.a).await.count, 0);
        // The stopped id is no longer an addressable fact for history either.
        let r = fact_history_for_session(&e.pool, &e.user, &e.key, &e.a, &city).await;
        assert_eq!(r.unwrap_err(), "fact_not_found");
    }

    /// Use again keeps the provenance columns (a copied-in fact stays copied-in), the
    /// policies and the source, and drops the stop marker.
    #[tokio::test]
    async fn use_again_copies_provenance_and_drops_the_marker() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        let id = create_entity_fact_with_provenance_conn(
            &mut c,
            Some(&maya),
            "City",
            "Leeds",
            "medical",
            "extract_confirm",
            &e.b,
            true,
            Some(&e.b),
            Some(serde_json::json!({"source_id": "x"})),
        )
        .await
        .unwrap();
        drop(c);
        stop_fact(&e, &e.a, &id).await.unwrap();
        again_fact(&e, &e.a, &id).await.unwrap();

        let mut c = conn(&e, &e.a).await;
        let row = sqlx::query(
            "SELECT sensitivity, source, source_persona_id, cross_persona_export,
                    origin_persona_id, extra_metadata, abstraction_tier2
             FROM entity_facts WHERE entity_id = ? AND valid_until IS NULL",
        )
        .bind(&maya)
        .fetch_one(&mut c)
        .await
        .unwrap();
        use sqlx::Row;
        assert_eq!(row.get::<String, _>("sensitivity"), "medical");
        assert_eq!(row.get::<String, _>("source"), "extract_confirm");
        assert_eq!(row.get::<String, _>("source_persona_id"), e.b);
        assert_eq!(row.get::<i64, _>("cross_persona_export"), 1);
        assert_eq!(
            row.get::<Option<String>, _>("origin_persona_id"),
            Some(e.b.clone())
        );
        let m: serde_json::Value =
            serde_json::from_str(&row.get::<String, _>("extra_metadata")).unwrap();
        assert_eq!(m["source_id"], "x");
        assert!(m.get("stopped_at").is_none() && m.get("stop_scope").is_none());
    }

    /// Use again is refused when a newer active value for the label exists, and when
    /// the stop was already used again (the older row is no longer the latest).
    #[tokio::test]
    async fn use_again_is_refused_for_a_newer_value_and_for_a_stale_stop() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        let first = fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        drop(c);
        stop_fact(&e, &e.a, &first).await.unwrap();
        // Someone writes a newer value for the same label.
        let mut c = conn(&e, &e.a).await;
        let newer = fact(&mut c, &e.a, Some(&maya), "City", "Bristol", "ipc").await;
        let before = count_facts(&mut c).await;
        drop(c);
        assert_eq!(
            again_fact(&e, &e.a, &first).await.unwrap_err(),
            "newer_value_exists"
        );
        let mut c = conn(&e, &e.a).await;
        assert_eq!(count_facts(&mut c).await, before, "nothing was written");
        drop(c);
        let t = tree(&e, &e.a).await.unwrap();
        assert_eq!(t.sections[0].facts[0].value, "Bristol");

        // Stale: stop the newer one, use the OLD stop again, then try the old id again.
        stop_fact(&e, &e.a, &newer).await.unwrap();
        again_fact(&e, &e.a, &newer).await.unwrap();
        stop_fact(
            &e,
            &e.a,
            &tree(&e, &e.a).await.unwrap().sections[0].facts[0].id,
        )
        .await
        .unwrap();
        assert_eq!(
            again_fact(&e, &e.a, &first).await.unwrap_err(),
            "fact_not_found",
            "an older stop is not the latest"
        );
        // Not a stop at all: a plain active or superseded row id.
        assert_eq!(
            again_fact(&e, &e.a, "no-such").await.unwrap_err(),
            "fact_not_found"
        );
    }

    /// Stopping twice is a clean not-found, and a superseded row is not stoppable.
    #[tokio::test]
    async fn stopping_twice_or_a_superseded_row_is_not_found() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let maya = entity(&mut c, "Maya", &[], None).await;
        let old = fact(&mut c, &e.a, Some(&maya), "City", "Leeds", "ipc").await;
        let new = fact(&mut c, &e.a, Some(&maya), "City", "Bristol", "ipc").await;
        drop(c);
        assert_eq!(
            stop_fact(&e, &e.a, &old).await.unwrap_err(),
            "fact_not_found"
        );
        stop_fact(&e, &e.a, &new).await.unwrap();
        assert_eq!(
            stop_fact(&e, &e.a, &new).await.unwrap_err(),
            "fact_not_found"
        );
        // A superseded (not stopped) row cannot be "used again" either.
        assert_eq!(
            again_fact(&e, &e.a, &old).await.unwrap_err(),
            "fact_not_found"
        );
    }

    /// A stopped section: the PARENT only is archived; its facts and its child's share
    /// one timestamp; no child status is written; and the Removed list shows the
    /// section with the whole count (the child is not listed alone).
    #[tokio::test]
    async fn stopping_a_section_archives_the_parent_only() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let h1 = fact(&mut c, &e.a, Some(&home), "Address", "1 High St", "ipc").await;
        let h2 = fact(&mut c, &e.a, Some(&home), "Size", "Large", "ipc").await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        let k1 = fact(&mut c, &e.a, Some(&kid), "Size", "Small", "ipc").await;
        drop(c);

        assert_eq!(stop_section(&e, &e.a, &home).await.unwrap(), 3);

        let mut c = conn(&e, &e.a).await;
        let parent: String = sqlx::query_scalar("SELECT status FROM entities WHERE id = ?")
            .bind(&home)
            .fetch_one(&mut c)
            .await
            .unwrap();
        let child: String = sqlx::query_scalar("SELECT status FROM entities WHERE id = ?")
            .bind(&kid)
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(parent, "archived");
        assert_eq!(child, "active", "children are not written (invariant 14)");
        let stamp = meta(&mut c, &h1).await["stopped_at"].clone();
        assert_eq!(
            meta(&mut c, &h2).await["stopped_at"],
            stamp,
            "one shared timestamp"
        );
        assert_eq!(meta(&mut c, &h1).await["stop_scope"], "section");
        let ent_meta: String =
            sqlx::query_scalar("SELECT extra_metadata FROM entities WHERE id = ?")
                .bind(&home)
                .fetch_one(&mut c)
                .await
                .unwrap();
        let ent_meta: serde_json::Value = serde_json::from_str(&ent_meta).unwrap();
        assert_eq!(ent_meta["stopped_at"], stamp);
        // The child's fact is ended with the same stamp and marker.
        assert_eq!(meta(&mut c, &k1).await["stopped_at"], stamp);
        assert_eq!(meta(&mut c, &k1).await["stop_scope"], "section");
        drop(c);

        assert!(tree(&e, &e.a).await.unwrap().sections.is_empty());
        let r = removed(&e, &e.a).await;
        assert_eq!(r.count, 1);
        assert!(r.facts.is_empty(), "its facts are listed with the section");
        assert_eq!(r.sections[0].name, "Home");
        assert_eq!(r.sections[0].fact_count, 3);
        assert_eq!(
            stop_section(&e, &e.a, &home).await.unwrap_err(),
            "section_not_found",
            "already stopped"
        );
    }

    /// What the context loader and the cross-Persona export list would read, by value.
    async fn read_by_loaders(e: &Env) -> (Vec<String>, Vec<String>) {
        use crate::persistence::personal_store::{
            list_pending_cross_persona_exports, load_entity_facts_for_context,
        };
        let mut context: Vec<String> = load_entity_facts_for_context(&e.user, &e.a, &e.key)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.field_value)
            .collect();
        let mut exports: Vec<String> = list_pending_cross_persona_exports(&e.user, &e.a, &e.key)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.field_value)
            .collect();
        context.sort();
        exports.sort();
        (context, exports)
    }

    /// A fact copied in from Persona B, so it is on the export list too.
    async fn copied_fact(
        c: &mut sqlx::SqliteConnection,
        from: &str,
        entity: &str,
        label: &str,
        value: &str,
    ) -> String {
        create_entity_fact_with_provenance_conn(
            c,
            Some(entity),
            label,
            value,
            "personal",
            "ipc",
            from,
            true,
            Some(from),
            None,
        )
        .await
        .unwrap()
    }

    async fn entity_rows(c: &mut sqlx::SqliteConnection) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT id, status, extra_metadata FROM entities ORDER BY id")
            .fetch_all(c)
            .await
            .unwrap()
    }

    /// Stopping a section stops its child's and grandchild's facts being READ (the
    /// context loader and the export list do not walk parents), writes no descendant's
    /// status or metadata, and use again brings them back.
    #[tokio::test]
    async fn a_section_stop_ends_descendant_facts_and_use_again_returns_them() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        let grandkid = entity(&mut c, "Shed", &[], Some(&kid)).await;
        let other = entity(&mut c, "Other", &[], None).await;
        copied_fact(&mut c, &e.b, &home, "Address", "HOME").await;
        copied_fact(&mut c, &e.b, &kid, "Size", "CHILD").await;
        copied_fact(&mut c, &e.b, &grandkid, "Lock", "GRANDCHILD").await;
        copied_fact(&mut c, &e.b, &other, "Note", "UNRELATED").await;
        sqlx::query("UPDATE entities SET modification_state = 'pristine'")
            .execute(&mut c)
            .await
            .unwrap();
        let before = entity_rows(&mut c).await;
        drop(c);
        let all = ["CHILD", "GRANDCHILD", "HOME", "UNRELATED"];
        let (context, exports) = read_by_loaders(&e).await;
        assert_eq!(context, all);
        assert_eq!(exports, all);

        // The count the confirm states is the count the stop ends.
        let t = tree(&e, &e.a).await.unwrap();
        let counts: Vec<_> = t
            .sections
            .iter()
            .map(|s| (s.name.as_str(), s.stop_fact_count))
            .collect();
        assert_eq!(
            counts,
            [("Home", 3), ("Garden", 2), ("Shed", 1), ("Other", 1)]
        );
        assert_eq!(stop_section(&e, &e.a, &home).await.unwrap(), 3);

        let (context, exports) = read_by_loaders(&e).await;
        assert_eq!(context, ["UNRELATED"]);
        assert_eq!(exports, ["UNRELATED"]);
        let r = removed(&e, &e.a).await;
        assert_eq!((r.count, r.sections[0].fact_count), (1, 3));
        assert!(r.facts.is_empty());

        let mut c = conn(&e, &e.a).await;
        let after = entity_rows(&mut c).await;
        for (b, a) in before.iter().zip(&after) {
            if b.0 == home {
                assert_eq!(a.1, "archived");
            } else {
                assert_eq!(a, b, "no descendant status or metadata is written");
            }
        }
        // Flipped so a persona sync holds them for review; the unrelated one is not.
        let states: std::collections::HashMap<String, String> =
            sqlx::query_as::<_, (String, String)>("SELECT id, modification_state FROM entities")
                .fetch_all(&mut c)
                .await
                .unwrap()
                .into_iter()
                .collect();
        for id in [&home, &kid, &grandkid] {
            assert_eq!(states[id], "user_modified");
        }
        assert_eq!(states[&other], "pristine");
        drop(c);

        assert_eq!(again_section(&e, &e.a, &home).await.unwrap(), 3);
        let (context, exports) = read_by_loaders(&e).await;
        assert_eq!(context, all);
        assert_eq!(exports, all);
        assert_eq!(removed(&e, &e.a).await.count, 0);
    }

    /// A child stopped earlier, with what is under it, stays stopped through the
    /// parent's stop and use again.
    #[tokio::test]
    async fn a_child_stopped_earlier_stays_stopped_through_the_parents_use_again() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        let grandkid = entity(&mut c, "Shed", &[], Some(&kid)).await;
        copied_fact(&mut c, &e.b, &home, "Address", "HOME").await;
        copied_fact(&mut c, &e.b, &kid, "Size", "CHILD").await;
        copied_fact(&mut c, &e.b, &grandkid, "Lock", "GRANDCHILD").await;
        drop(c);

        assert_eq!(stop_section(&e, &e.a, &kid).await.unwrap(), 2);
        assert_eq!(stop_section(&e, &e.a, &home).await.unwrap(), 1);
        assert!(read_by_loaders(&e).await.0.is_empty());
        assert_eq!(again_section(&e, &e.a, &home).await.unwrap(), 1);

        let (context, exports) = read_by_loaders(&e).await;
        assert_eq!(context, ["HOME"]);
        assert_eq!(exports, ["HOME"]);
        let r = removed(&e, &e.a).await;
        assert_eq!(r.sections.len(), 1);
        assert_eq!(
            (r.sections[0].name.as_str(), r.sections[0].fact_count),
            ("Garden", 2)
        );
        assert_eq!(again_section(&e, &e.a, &kid).await.unwrap(), 2);
        assert_eq!(read_by_loaders(&e).await.0, ["CHILD", "GRANDCHILD", "HOME"]);
    }

    /// Use again on a section is refused, writing nothing, while any ancestor is
    /// stopped or owned: a child's facts never come back active under a stopped
    /// parent. It works once the ancestors are visible again.
    #[tokio::test]
    async fn a_section_cannot_come_back_under_a_stopped_or_owned_ancestor() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        let grandkid = entity(&mut c, "Shed", &[], Some(&kid)).await;
        copied_fact(&mut c, &e.b, &home, "Address", "HOME").await;
        copied_fact(&mut c, &e.b, &grandkid, "Lock", "GRANDCHILD").await;
        drop(c);
        // The grandchild on its own first, then Home: Garden between them stays active.
        assert_eq!(stop_section(&e, &e.a, &grandkid).await.unwrap(), 1);
        assert_eq!(stop_section(&e, &e.a, &home).await.unwrap(), 1);

        let snapshot = |e: &Env| {
            let (u, a, k) = (e.user.clone(), e.a.clone(), e.key.clone());
            async move {
                let mut c = open_personal_db(&u, &a, &k).await.unwrap();
                let facts: Vec<(String, Option<String>, String)> = sqlx::query_as(
                    "SELECT id, valid_until, extra_metadata FROM entity_facts ORDER BY id",
                )
                .fetch_all(&mut c)
                .await
                .unwrap();
                (facts, entity_rows(&mut c).await)
            }
        };
        let before = snapshot(&e).await;
        // A grandparent is stopped, not the direct parent.
        assert_eq!(
            again_section(&e, &e.a, &grandkid).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(snapshot(&e).await, before, "nothing written");
        assert!(read_by_loaders(&e).await.0.is_empty());

        assert_eq!(again_section(&e, &e.a, &home).await.unwrap(), 1);
        assert_eq!(again_section(&e, &e.a, &grandkid).await.unwrap(), 1);
        assert_eq!(read_by_loaders(&e).await.0, ["GRANDCHILD", "HOME"]);

        // A direct parent that is stopped, and one that is Focus-owned.
        stop_section(&e, &e.a, &grandkid).await.unwrap();
        stop_section(&e, &e.a, &kid).await.unwrap();
        let before = snapshot(&e).await;
        assert_eq!(
            again_section(&e, &e.a, &grandkid).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(snapshot(&e).await, before, "nothing written");
        again_section(&e, &e.a, &kid).await.unwrap();
        let mut c = conn(&e, &e.a).await;
        let dev = owned(&mut c, "Router").await;
        sqlx::query("UPDATE entities SET parent_entity_id = ? WHERE id = ?")
            .bind(&dev)
            .bind(&grandkid)
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);
        let before = snapshot(&e).await;
        assert_eq!(
            again_section(&e, &e.a, &grandkid).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(snapshot(&e).await, before, "nothing written");
    }

    /// A conflict in a DESCENDANT refuses the whole restore; a descendant moved since
    /// the stop still comes back (the facts are found by the shared timestamp).
    #[tokio::test]
    async fn descendant_restore_is_atomic_and_follows_the_stamp_not_the_chain() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        fact(&mut c, &e.a, Some(&home), "Address", "HOME", "ipc").await;
        fact(&mut c, &e.a, Some(&kid), "Size", "CHILD", "ipc").await;
        drop(c);
        stop_section(&e, &e.a, &home).await.unwrap();
        let mut c = conn(&e, &e.a).await;
        let newer = fact(&mut c, &e.a, Some(&kid), "Size", "NEWER", "ipc").await;
        let before = count_facts(&mut c).await;
        drop(c);
        assert_eq!(
            again_section(&e, &e.a, &home).await.unwrap_err(),
            "newer_value_exists"
        );
        let mut c = conn(&e, &e.a).await;
        assert_eq!(count_facts(&mut c).await, before, "no partial restore");
        sqlx::query("UPDATE entity_facts SET valid_until = 'later' WHERE id = ?")
            .bind(&newer)
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("UPDATE entities SET parent_entity_id = NULL WHERE id = ?")
            .bind(&kid)
            .execute(&mut c)
            .await
            .unwrap();
        drop(c);
        assert_eq!(again_section(&e, &e.a, &home).await.unwrap(), 2);
        assert_eq!(read_by_loaders(&e).await.0, ["CHILD", "HOME"]);
    }

    /// The descendant walk never goes into a Focus-owned entity, and the stated count
    /// does not include its facts.
    #[tokio::test]
    async fn a_section_stop_never_writes_an_owned_descendant() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        fact(&mut c, &e.a, Some(&home), "Address", "HOME", "ipc").await;
        let dev = owned(&mut c, "Router").await;
        sqlx::query("UPDATE entities SET parent_entity_id = ? WHERE id = ?")
            .bind(&home)
            .bind(&dev)
            .execute(&mut c)
            .await
            .unwrap();
        let dev_fact = fact(&mut c, &e.a, Some(&dev), "firmware", "FW", "ipc").await;
        drop(c);
        assert_eq!(tree(&e, &e.a).await.unwrap().sections[0].stop_fact_count, 1);
        assert_eq!(stop_section(&e, &e.a, &home).await.unwrap(), 1);
        let mut c = conn(&e, &e.a).await;
        let until: Option<String> =
            sqlx::query_scalar("SELECT valid_until FROM entity_facts WHERE id = ?")
                .bind(&dev_fact)
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert!(
            until.is_none(),
            "an owned entity's fact is never ended here"
        );
    }

    /// Use again on a section: status active, marker gone, a new row per fact stopped
    /// with it. A fact stopped on its own first stays stopped; a child stopped on its
    /// own stays stopped when the parent returns.
    #[tokio::test]
    async fn using_a_section_again_restores_only_what_was_stopped_with_it() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        fact(&mut c, &e.a, Some(&home), "Address", "1 High St", "ipc").await;
        let size = fact(&mut c, &e.a, Some(&home), "Size", "Large", "ipc").await;
        let kid = entity(&mut c, "Garden", &[], Some(&home)).await;
        fact(&mut c, &e.a, Some(&kid), "Size", "Small", "ipc").await;
        drop(c);
        // The user first stops one fact alone, then the child on its own, then Home.
        stop_fact(&e, &e.a, &size).await.unwrap();
        assert_eq!(stop_section(&e, &e.a, &kid).await.unwrap(), 1);
        assert_eq!(
            stop_section(&e, &e.a, &home).await.unwrap(),
            1,
            "Address only"
        );

        // The child sits under a stopped parent: not listed on its own.
        let r = removed(&e, &e.a).await;
        assert_eq!(r.sections.len(), 1);
        assert_eq!(r.sections[0].fact_count, 1);

        assert_eq!(again_section(&e, &e.a, &home).await.unwrap(), 1);

        let t = tree(&e, &e.a).await.unwrap();
        assert_eq!(names(&t), ["Home"], "the child stays stopped");
        let labels: Vec<_> = t.sections[0]
            .facts
            .iter()
            .map(|f| f.label.as_str())
            .collect();
        assert_eq!(labels, ["Address"], "the fact stopped alone stays stopped");
        let r = removed(&e, &e.a).await;
        assert!(r.facts.iter().any(|f| f.id == size && f.label == "Size"));
        assert!(r.sections.iter().any(|s| s.name == "Garden"));
        assert_eq!(r.count, 2);
        let mut c = conn(&e, &e.a).await;
        let ent_meta: String =
            sqlx::query_scalar("SELECT extra_metadata FROM entities WHERE id = ?")
                .bind(&home)
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert!(!ent_meta.contains("stopped_at"));
        drop(c);
        // And the child can be brought back on its own.
        assert_eq!(again_section(&e, &e.a, &kid).await.unwrap(), 1);
        assert_eq!(names(&tree(&e, &e.a).await.unwrap()), ["Home", "Garden"]);
    }

    /// A conflict on section restore refuses the whole thing and writes nothing.
    #[tokio::test]
    async fn using_a_section_again_is_atomic_on_a_conflict() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let home = entity(&mut c, "Home", &[], None).await;
        fact(&mut c, &e.a, Some(&home), "Address", "1 High St", "ipc").await;
        fact(&mut c, &e.a, Some(&home), "Size", "Large", "ipc").await;
        drop(c);
        stop_section(&e, &e.a, &home).await.unwrap();
        let mut c = conn(&e, &e.a).await;
        // A newer active value for one label appears while the section is stopped.
        fact(&mut c, &e.a, Some(&home), "Size", "Huge", "ipc").await;
        let before = count_facts(&mut c).await;
        drop(c);
        assert_eq!(
            again_section(&e, &e.a, &home).await.unwrap_err(),
            "newer_value_exists"
        );
        let mut c = conn(&e, &e.a).await;
        assert_eq!(count_facts(&mut c).await, before, "no partial restore");
        let status: String = sqlx::query_scalar("SELECT status FROM entities WHERE id = ?")
            .bind(&home)
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(status, "archived");
    }

    /// A section archived by something else (no stop marker) is neither listed nor
    /// restorable here, and is not stoppable once archived.
    #[tokio::test]
    async fn a_section_archived_elsewhere_is_not_ours_to_restore() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let other = entity(&mut c, "Archived Elsewhere", &[], None).await;
        entity_store::retire_entity_conn(&mut c, &other, "archived")
            .await
            .unwrap();
        drop(c);
        assert_eq!(removed(&e, &e.a).await.count, 0);
        assert_eq!(
            again_section(&e, &e.a, &other).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(
            stop_section(&e, &e.a, &other).await.unwrap_err(),
            "section_not_found"
        );
    }

    /// OWNED entities are refused on EVERY write path and in the Removed list, and
    /// nothing changes (decisions.id=945).
    #[tokio::test]
    async fn owned_entities_are_refused_on_every_write_path() {
        let e = env().await;
        let mut c = conn(&e, &e.a).await;
        let dev = owned(&mut c, "Netgear Router").await;
        let dev_fact = fact(&mut c, &e.a, Some(&dev), "firmware", "FW-1", "ipc").await;
        // An owned entity that is stopped (marker forged by hand) with a stopped fact.
        let gone = owned(&mut c, "Old Device").await;
        let gone_fact = fact(&mut c, &e.a, Some(&gone), "firmware", "FW-0", "ipc").await;
        sqlx::query(
            "UPDATE entity_facts SET valid_until = 't1',
             extra_metadata = '{\"stopped_at\":\"t1\",\"stop_scope\":\"fact\"}' WHERE id = ?",
        )
        .bind(&gone_fact)
        .execute(&mut c)
        .await
        .unwrap();
        let sect_fact = fact(&mut c, &e.a, Some(&gone), "make", "Acme", "ipc").await;
        sqlx::query(
            "UPDATE entity_facts SET valid_until = 't2',
             extra_metadata = '{\"stopped_at\":\"t2\",\"stop_scope\":\"section\"}' WHERE id = ?",
        )
        .bind(&sect_fact)
        .execute(&mut c)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE entities SET status = 'archived', extra_metadata = '{\"stopped_at\":\"t2\"}' WHERE id = ?",
        )
        .bind(&gone)
        .execute(&mut c)
        .await
        .unwrap();
        let snapshot = |rows: Vec<sqlx::sqlite::SqliteRow>| {
            use sqlx::Row;
            rows.iter()
                .map(|r| {
                    format!(
                        "{}|{:?}|{}",
                        r.get::<String, _>("id"),
                        r.get::<Option<String>, _>("valid_until"),
                        r.get::<String, _>("extra_metadata")
                    )
                })
                .collect::<Vec<_>>()
        };
        let facts_before = snapshot(
            sqlx::query("SELECT id, valid_until, extra_metadata FROM entity_facts ORDER BY id")
                .fetch_all(&mut c)
                .await
                .unwrap(),
        );
        let ents_before: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, status, extra_metadata FROM entities ORDER BY id")
                .fetch_all(&mut c)
                .await
                .unwrap();
        drop(c);

        assert_eq!(
            stop_fact(&e, &e.a, &dev_fact).await.unwrap_err(),
            "fact_not_found"
        );
        assert_eq!(
            stop_section(&e, &e.a, &dev).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(
            again_fact(&e, &e.a, &gone_fact).await.unwrap_err(),
            "fact_not_found"
        );
        assert_eq!(
            again_section(&e, &e.a, &gone).await.unwrap_err(),
            "section_not_found"
        );
        let r = removed(&e, &e.a).await;
        assert_eq!(r.count, 0, "an owned entity is never listed as removed");

        let mut c = conn(&e, &e.a).await;
        let facts_after = snapshot(
            sqlx::query("SELECT id, valid_until, extra_metadata FROM entity_facts ORDER BY id")
                .fetch_all(&mut c)
                .await
                .unwrap(),
        );
        let ents_after: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, status, extra_metadata FROM entities ORDER BY id")
                .fetch_all(&mut c)
                .await
                .unwrap();
        assert_eq!(facts_before, facts_after, "no fact row changed");
        assert_eq!(ents_before, ents_after, "no entity row changed");
    }

    /// A fact or section id from another Persona is refused on every write, and the
    /// other Persona's file is untouched.
    #[tokio::test]
    async fn another_personas_ids_are_refused_on_every_write() {
        let e = env().await;
        let mut ca = conn(&e, &e.a).await;
        fact(&mut ca, &e.a, None, "location_city", "Leeds", "ipc").await;
        drop(ca);
        let mut cb = conn(&e, &e.b).await;
        let zoe = entity(&mut cb, "ZoeOnlyInB", &[], None).await;
        let b_fact = fact(&mut cb, &e.b, Some(&zoe), "Vet", "Parkside", "ipc").await;
        let b_single = fact(&mut cb, &e.b, None, "job_title", "Nurse", "ipc").await;
        // B also has a stopped fact and a stopped section of its own.
        let gone = entity(&mut cb, "GoneInB", &[], None).await;
        let gone_fact = fact(&mut cb, &e.b, Some(&gone), "Note", "x", "ipc").await;
        drop(cb);
        stop_fact(&e, &e.b, &b_single).await.unwrap();
        stop_section(&e, &e.b, &gone).await.unwrap();
        let mut cb = conn(&e, &e.b).await;
        let before: Vec<(String, Option<String>, String)> =
            sqlx::query_as("SELECT id, valid_until, extra_metadata FROM entity_facts ORDER BY id")
                .fetch_all(&mut cb)
                .await
                .unwrap();
        let ents_before: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, status, extra_metadata FROM entities ORDER BY id")
                .fetch_all(&mut cb)
                .await
                .unwrap();
        drop(cb);

        // Through A, none of B's ids resolve.
        assert_eq!(
            stop_fact(&e, &e.a, &b_fact).await.unwrap_err(),
            "fact_not_found"
        );
        assert_eq!(
            stop_section(&e, &e.a, &zoe).await.unwrap_err(),
            "section_not_found"
        );
        assert_eq!(
            again_fact(&e, &e.a, &b_single).await.unwrap_err(),
            "fact_not_found"
        );
        assert_eq!(
            again_section(&e, &e.a, &gone).await.unwrap_err(),
            "section_not_found"
        );
        // B's stopped items are not in A's Removed list.
        assert_eq!(removed(&e, &e.a).await.count, 0);
        assert!(
            removed(&e, &e.b).await.count > 0,
            "so the empty list is isolation"
        );
        let _ = gone_fact;

        let mut cb = conn(&e, &e.b).await;
        let after: Vec<(String, Option<String>, String)> =
            sqlx::query_as("SELECT id, valid_until, extra_metadata FROM entity_facts ORDER BY id")
                .fetch_all(&mut cb)
                .await
                .unwrap();
        let ents_after: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, status, extra_metadata FROM entities ORDER BY id")
                .fetch_all(&mut cb)
                .await
                .unwrap();
        assert_eq!(before, after, "B's facts were not touched through A");
        assert_eq!(
            ents_before, ents_after,
            "B's sections were not touched through A"
        );
    }

    /// Non-member, malformed and wrong-key Personas refuse every write, writing nothing.
    #[tokio::test]
    async fn writes_need_membership_and_a_readable_file() {
        let e = env().await;
        let mut c = conn(&e, &e.stranger_persona).await;
        let sf = fact(
            &mut c,
            &e.stranger_persona,
            None,
            "location_city",
            "Elsewhere",
            "ipc",
        )
        .await;
        drop(c);
        for p in [
            e.stranger_persona.as_str(),
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert_eq!(
                stop_fact(&e, p, &sf).await.unwrap_err(),
                "persona_not_found"
            );
            assert_eq!(
                stop_section(&e, p, "x").await.unwrap_err(),
                "persona_not_found"
            );
            assert_eq!(
                again_fact(&e, p, &sf).await.unwrap_err(),
                "persona_not_found"
            );
            assert_eq!(
                again_section(&e, p, "x").await.unwrap_err(),
                "persona_not_found"
            );
            let r = list_removed_for_session(&e.pool, &e.user, &e.key, p).await;
            assert_eq!(r.unwrap_err(), "persona_not_found");
        }
        let mut c = conn(&e, &e.stranger_persona).await;
        let until: Option<String> =
            sqlx::query_scalar("SELECT valid_until FROM entity_facts WHERE id = ?")
                .bind(&sf)
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert!(until.is_none(), "the stranger's fact is untouched");
        drop(c);

        // Persona A's file must exist (created with the real key) for a wrong key to
        // be a wrong key rather than a new empty file.
        let mut c = conn(&e, &e.a).await;
        fact(&mut c, &e.a, None, "location_city", "Leeds", "ipc").await;
        drop(c);
        let bad = key_hex(&OTHER_KEY);
        let r = stop_using_fact_for_session(&e.pool, &e.user, &bad, &e.a, "x").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = stop_using_section_for_session(&e.pool, &e.user, &bad, &e.a, "x").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = use_again_fact_for_session(&e.pool, &e.user, &bad, &e.a, "x").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = use_again_section_for_session(&e.pool, &e.user, &bad, &e.a, "x").await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
        let r = list_removed_for_session(&e.pool, &e.user, &bad, &e.a).await;
        assert_eq!(r.unwrap_err(), "facts_unreadable");
    }

    /// The unit 2 commands need a session.
    #[tokio::test]
    async fn the_write_commands_need_a_session() {
        let e = env().await;
        let app = mock_app_with_registry(e.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();
        let (a, s) = (e.a.clone(), "x".to_owned());
        assert!(list_removed(a.clone(), registry.clone(), pool.clone())
            .await
            .is_err());
        assert!(
            stop_using_fact(a.clone(), s.clone(), registry.clone(), pool.clone())
                .await
                .is_err()
        );
        assert!(
            stop_using_section(a.clone(), s.clone(), registry.clone(), pool.clone())
                .await
                .is_err()
        );
        assert!(
            use_again_fact(a.clone(), s.clone(), registry.clone(), pool.clone())
                .await
                .is_err()
        );
        assert!(use_again_section(a, s, registry.clone(), pool.clone())
            .await
            .is_err());
    }

    /// No DELETE statement exists in any module that writes the Facts view, and no
    /// command or store reads or writes entity_facts outside the owner-checked path.
    #[test]
    fn nothing_that_writes_the_facts_view_can_delete() {
        for (name, src) in [
            ("my_facts.rs", include_str!("my_facts.rs")),
            (
                "facts_stop_store.rs",
                include_str!("../persistence/facts_stop_store.rs"),
            ),
            (
                "facts_view_store.rs",
                include_str!("../persistence/facts_view_store.rs"),
            ),
        ] {
            let code: String = production(src)
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let upper = code.to_uppercase();
            assert!(!upper.contains("DELETE"), "{name}: a DELETE");
            assert!(!upper.contains("DROP "), "{name}: a DROP");
            assert!(!upper.contains("TRUNCATE"), "{name}: a TRUNCATE");
        }
    }

    /// Every direct write on the entities table carries the owner filter, every write
    /// resolves its id in the visible set (or the owner-filtered SELECT) first, and the
    /// provenance columns are never assigned by an UPDATE.
    #[test]
    fn the_write_store_is_owner_filtered_and_never_touches_provenance() {
        let code: String = production(include_str!("../persistence/facts_stop_store.rs"))
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut n = 0;
        for marker in ["FROM entities", "UPDATE entities"] {
            for part in code.split(marker).skip(1) {
                let window: String = part.chars().take(400).collect();
                assert!(
                    window.contains("owner_focus IS NULL"),
                    "{marker} without the owner filter: {window}"
                );
                n += 1;
            }
        }
        assert_eq!(
            n, 5,
            "two entities UPDATEs, the descendant walk and the two restore SELECTs"
        );
        for col in [
            "source_persona_id =",
            "cross_persona_export =",
            "origin_persona_id =",
        ] {
            assert!(!code.contains(col), "an UPDATE assigns {col}");
        }
        // The stop and the restore each check the visible set before any write.
        for f in [
            "stop_fact_conn",
            "stop_section_conn",
            "use_again_fact_conn",
            "use_again_section_conn",
        ] {
            let start = code.find(&format!("pub async fn {f}")).unwrap();
            // The joined lines have no trailing newline, and the last function ends the file.
            let body = format!("{}\n", &code[start..]);
            let end = body.find("\n}\n").unwrap();
            let body = &body[..end];
            let guard = body.find("visible_ids(conn)").expect("visible set");
            let write = body.find("atomic!").expect("a write");
            assert!(guard < write, "{f}: the visible set comes before the write");
        }
        // Every one of the eight _for_session bodies opens through open_checked.
        let src = production(include_str!("my_facts.rs"));
        assert_eq!(
            src.matches("open_checked(pool, user_id, key_hex, persona_id)")
                .count(),
            8
        );
    }
}

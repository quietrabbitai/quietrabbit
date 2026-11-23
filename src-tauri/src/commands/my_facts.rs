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
// Add, change, stop using and use again are NOT here (items.id=961 and the later
// units of items.id=959).

use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::persistence::facts_view_store::{
    self as store, EarlierValue, FactsTree, FactsViewError,
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
    store::facts_tree_conn(&mut conn).await.map_err(code)
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
    store::search_facts_conn(&mut conn, query)
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
        assert_eq!(commands.len(), 3, "the unit 1 read set, no more");
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
        assert_eq!(reads, 2, "the section loader and the alias search");
        assert!(!src.contains("entity_store::"), "no entity_store reader");
        assert!(
            !src.contains("search_entities"),
            "search_entities not widened"
        );
    }
}

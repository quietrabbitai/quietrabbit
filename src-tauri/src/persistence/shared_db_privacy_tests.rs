// src-tauri/src/persistence/shared_db_privacy_tests.rs
//
// items.id=661 (decisions.id=862), DONE WHEN: a fresh install's shared.db,
// opened without a key, contains no Persona names or types, group names,
// sync folder paths or invitation labels, and its table and column set is
// exactly the residual list in shared_db_residual.rs.
//
// Two tests:
//   - the SCAN: drive every real writer that used to put user text in shared.db
//     with a distinctive sentinel, then read the file's raw bytes (and its WAL
//     and SHM sidecars) and every cell through an UNKEYED connection, and fail
//     if a sentinel is anywhere. A control proves the scanner finds one.
//   - the PIN: the real schema equals the classified list.
//
// The scan can only prove the writers it drives; the pin is what catches a new
// column. Together they are the CI form of the floor carve-out.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, Row};

use crate::auth::kdf;
use crate::auth::persona_sharing::{self, ShareType};
use crate::auth::registry::{key_hex, GroupKeyRegistry, KeyRegistry, UnlockedKey};
use crate::auth::sharing_keypair;
use crate::auth::{group_creation, group_invitations, group_membership};
use crate::group_sync::settings_store::GroupSyncCtx;
use crate::persistence::shared_db_residual::RESIDUAL_SHARED_SCHEMA;
use crate::persistence::user_provider_preference_store::{
    upsert_preference, NewUserProviderPreference, UserPreference,
};
use crate::persistence::{persona_store, provider_spend_store, topic_store};
use crate::test_support::ENV_MUTEX;

const NEEDLE: &str = "SENTINEL";

struct RootGuard(Option<String>);

impl RootGuard {
    fn set(path: &Path) -> Self {
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::set_var("QR_DATA_ROOT", path);
        RootGuard(saved)
    }
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

/// The file plus its WAL and SHM sidecars.
fn db_files(path: &Path) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        files.push(PathBuf::from(name));
    }
    files
}

/// Raw-byte scan of the database file and its sidecars. Returns the files that
/// contain the needle, in plaintext (ASCII, case-insensitive) or as lowercase
/// hex, which is how a text column would look if it were stored as hex.
fn raw_hits(path: &Path, needle: &str) -> Vec<String> {
    let lower = needle.to_ascii_lowercase();
    let hex: String = needle.bytes().map(|b| format!("{b:02x}")).collect();
    let mut hits = Vec::new();
    for file in db_files(path) {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        let haystack = bytes.to_ascii_lowercase();
        let has = |n: &[u8]| !n.is_empty() && haystack.windows(n.len()).any(|w| w == n);
        if has(lower.as_bytes()) || has(hex.as_bytes()) {
            hits.push(file.display().to_string());
        }
    }
    hits
}

/// Every cell of every table, read through a connection that supplies no key.
/// Returns "table.column" for each cell containing the needle.
async fn cell_hits(path: &Path, needle: &str) -> Vec<String> {
    let lower = needle.to_ascii_lowercase();
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .expect("an unkeyed connection must open shared.db");
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();

    let mut hits = Vec::new();
    for (table,) in tables {
        let rows = sqlx::query(&format!("SELECT * FROM \"{table}\""))
            .fetch_all(&mut conn)
            .await
            .unwrap();
        for row in rows {
            for (i, column) in sqlx::Row::columns(&row).iter().enumerate() {
                let text: Option<String> = row.try_get(i).ok();
                let blob: Option<Vec<u8>> = row.try_get(i).ok();
                let cell = text
                    .map(|t| t.to_ascii_lowercase())
                    .or_else(|| blob.map(|b| String::from_utf8_lossy(&b).to_ascii_lowercase()));
                if cell.is_some_and(|c| c.contains(&lower)) {
                    hits.push(format!("{table}.{}", sqlx::Column::name(column)));
                }
            }
        }
    }
    conn.close().await.unwrap();
    hits
}

/// The scanner must find a needle that is there, or a clean result means
/// nothing.
#[tokio::test]
async fn the_scanner_finds_a_needle_that_is_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.db");
    let mut conn = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::query("CREATE TABLE t (a TEXT, b BLOB)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES ('x SENTINEL y', NULL), ('clean', X'53454e54494e454c')")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    assert!(!raw_hits(&path, NEEDLE).is_empty());
    let cells = cell_hits(&path, NEEDLE).await;
    assert!(cells.contains(&"t.a".to_owned()), "{cells:?}");
    assert!(cells.contains(&"t.b".to_owned()), "{cells:?}");
}

struct Account {
    user_id: String,
    master_key: [u8; kdf::MASTER_KEY_LEN],
    key_hex: String,
    sharing_private: x25519_dalek::StaticSecret,
    persona_id: String,
}

async fn make_account(
    pool: &sqlx::SqlitePool,
    name: &str,
    fill: u8,
    persona_name: &str,
) -> Account {
    let user_id = uuid::Uuid::new_v4().to_string();
    let master_key = [fill; kdf::MASTER_KEY_LEN];
    let (sharing_private, sharing_public) =
        sharing_keypair::derive_sharing_keypair(&master_key, &user_id, 1);
    crate::auth::user_store::create_user(
        pool,
        &user_id,
        name,
        "user",
        false,
        b"test-salt",
        1024,
        1,
        1,
        sharing_public.as_bytes(),
    )
    .await
    .expect("create_user");
    let key_hex = key_hex(&master_key);
    let persona_id = uuid::Uuid::new_v4().to_string();
    persona_store::create_persona(
        pool,
        &persona_id,
        persona_name,
        "protected",
        &user_id,
        &key_hex,
        Some("SENTINEL-COLOR"),
    )
    .await
    .expect("create_persona");
    Account {
        user_id,
        master_key,
        key_hex,
        sharing_private,
        persona_id,
    }
}

async fn log_in(account: &Account) -> KeyRegistry {
    let registry = KeyRegistry::default();
    registry
        .replace(UnlockedKey {
            user_id: account.user_id.clone(),
            data_key: account.master_key,
            key_epoch: 1,
            sharing_private_key: account.sharing_private.to_bytes(),
            unlocked_at: crate::providers::utils::now(),
        })
        .await;
    registry
}

#[tokio::test]
async fn a_fresh_installs_shared_db_contains_no_names_types_paths_or_labels() {
    let _lock = ENV_MUTEX.lock().await;
    let tempdir = tempfile::tempdir().unwrap();
    let _root = RootGuard::set(tempdir.path());

    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("migrate shared.db");
    let shared_path = crate::providers::utils::db_path_shared();
    let pool = sqlx::SqlitePool::connect_with(
        crate::providers::utils::connect_options_unencrypted(&shared_path),
    )
    .await
    .unwrap();

    // Three accounts: a sender and two members. Every Persona has a sentinel
    // name and type, which create_persona writes to the encrypted registry.
    let alice = make_account(&pool, "Alice", 0x71, "SENTINEL-PERSONA-NAME-A").await;
    let bob = make_account(&pool, "Bob", 0x72, "SENTINEL-PERSONA-NAME-B").await;
    let carol = make_account(&pool, "Carol", 0x73, "SENTINEL-PERSONA-NAME-C").await;

    // Group invitations and a rotation, with sentinel names and labels.
    let group_id = "group-scan-1";
    let group_key = [0x55u8; kdf::MASTER_KEY_LEN];
    let registry = GroupKeyRegistry::default();
    for member in [&bob, &carol] {
        let invitation = group_invitations::send_invitation(
            &pool,
            &member.persona_id,
            group_id,
            "SENTINEL-GROUP-NAME",
            "SENTINEL-SENDER-LABEL",
            &group_key,
        )
        .await
        .expect("send_invitation");
        group_invitations::accept_invitation(
            &pool,
            &invitation,
            &member.persona_id,
            &member.sharing_private,
            &registry,
            &member.key_hex,
        )
        .await
        .expect("accept_invitation");
    }
    group_membership::remove_member(
        &pool,
        group_id,
        &bob.persona_id,
        group_membership::DepartureReason::Removed,
        "SENTINEL-ROTATION-LABEL",
        &registry,
        &KeyRegistry::default(),
    )
    .await
    .expect("remove_member");

    // A group the logged-in account creates itself.
    let alice_session = log_in(&alice).await;
    group_creation::create_group(
        &pool,
        &alice.persona_id,
        "SENTINEL-CREATED-GROUP",
        "SENTINEL-CREATOR-LABEL",
        &GroupKeyRegistry::default(),
        &alice_session,
    )
    .await
    .expect("create_group");

    // Floor consent, then a Persona share (name and type travel in the payload).
    crate::persistence::personal_store::write_floor_consent_preference(
        &alice.user_id,
        &alice.persona_id,
        &alice.key_hex,
        2,
    )
    .await
    .unwrap();
    persona_sharing::send_persona_share(
        &pool,
        &alice.user_id,
        &alice.persona_id,
        &alice.key_hex,
        &bob.user_id,
        ShareType::Synced,
    )
    .await
    .expect("send_persona_share");

    // The three folder-sync settings, each with an error that embeds the path.
    let ctx = GroupSyncCtx {
        user_id: &alice.user_id,
        data_key_hex: &alice.key_hex,
    };
    crate::group_sync::settings_store::set_group_sync_folder(
        &ctx,
        &alice.persona_id,
        group_id,
        "/SENTINEL/group/path",
    )
    .await
    .unwrap();
    crate::group_sync::settings_store::record_sync_result(
        &ctx,
        &alice.persona_id,
        group_id,
        Err("cannot open /SENTINEL/group/path"),
    )
    .await
    .unwrap();
    crate::persona_sync::settings_store::set_persona_share_sync_folder(
        &alice.user_id,
        &alice.key_hex,
        &alice.persona_id,
        "share-scan-1",
        crate::persona_sync::settings_store::SyncRole::Owner,
        "/SENTINEL/share/path",
    )
    .await
    .unwrap();
    crate::persona_view_sync::settings_store::set_persona_view_share_sync_folder(
        &bob.user_id,
        &bob.key_hex,
        "share-scan-2",
        "/SENTINEL/view/path",
    )
    .await
    .unwrap();

    // A provider preference with a plan and a model.
    let mut keys = provider_spend_store::open(&alice.user_id, &alice.key_hex)
        .await
        .unwrap();
    upsert_preference(
        &mut keys,
        NewUserProviderPreference {
            user_id: &alice.user_id,
            persona_id: Some(&alice.persona_id),
            focus_id: Some("SENTINEL-FOCUS"),
            provider_id: "SENTINEL-PROVIDER",
            login_available: true,
            user_preference: UserPreference::Preferred,
            local_model_version: None,
            subscription_status: None,
            plan_key: Some("SENTINEL-PLAN"),
            model_id: Some("SENTINEL-MODEL"),
        },
    )
    .await
    .unwrap();
    drop(keys);

    // A named topic.
    let topic = topic_store::create_topic(
        &alice.user_id,
        &alice.persona_id,
        &alice.key_hex,
        "SENTINEL-FOCUS-ID",
        Some("SENTINEL-TOPIC-NAME"),
        None,
    )
    .await
    .expect("create_topic");
    topic_store::name_topic(
        &alice.user_id,
        &alice.persona_id,
        &alice.key_hex,
        &topic.id,
        "SENTINEL-TOPIC-RENAMED",
    )
    .await
    .unwrap();

    // Validity: the sentinels really were written, in the encrypted stores.
    let listed = persona_store::list_personas_for_user(&pool, &alice.user_id, &alice.key_hex)
        .await
        .unwrap();
    assert!(listed
        .iter()
        .any(|p| p.display_name == "SENTINEL-PERSONA-NAME-A"));
    let invites = group_invitations::list_pending_invitations(
        &pool,
        &carol.persona_id,
        &carol.sharing_private,
    )
    .await
    .unwrap();
    assert!(
        invites.is_empty(),
        "Carol accepted hers, so nothing is pending"
    );

    // Close every connection so the WAL is checkpointed, then scan.
    pool.close().await;

    let raw = raw_hits(&shared_path, NEEDLE);
    assert!(
        raw.is_empty(),
        "shared.db bytes contain a sentinel in: {raw:?}"
    );
    let cells = cell_hits(&shared_path, NEEDLE).await;
    assert!(
        cells.is_empty(),
        "unkeyed read of shared.db finds a sentinel in: {cells:?}"
    );
}

#[tokio::test]
async fn the_shared_db_schema_is_exactly_the_residual_list() {
    let mut conn = SqliteConnectOptions::new()
        .filename(":memory:")
        .connect()
        .await
        .unwrap();
    crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
        .await
        .unwrap();

    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let mut actual: BTreeSet<(String, String)> = BTreeSet::new();
    for (table,) in &tables {
        let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(&mut conn)
            .await
            .unwrap();
        for (c,) in cols {
            actual.insert((table.clone(), c));
        }
    }

    let mut listed: BTreeSet<(String, String)> = BTreeSet::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for t in RESIDUAL_SHARED_SCHEMA {
        assert!(
            names.insert(t.name),
            "{} appears twice in the residual list",
            t.name
        );
        for c in t.columns {
            listed.insert((t.name.to_owned(), (*c).to_owned()));
        }
    }

    let unlisted: Vec<_> = actual.difference(&listed).collect();
    let missing: Vec<_> = listed.difference(&actual).collect();
    assert!(
        unlisted.is_empty() && missing.is_empty(),
        "shared.db and persistence/shared_db_residual.rs disagree.\n\
         In shared.db but not classified (classify it, or move it to an encrypted store): \
         {unlisted:?}\nClassified but not in shared.db: {missing:?}"
    );
}

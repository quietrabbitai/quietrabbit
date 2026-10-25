// Engine tests (items.id=630 stage C). Every test builds a real account in a
// tempdir: shared.db with a wrapper, and real encrypted databases and blobs
// under the account directory. Crashes are simulated with FaultPoint, which
// leaves state exactly as a dead process would.

use std::sync::Mutex;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

use super::*;
use crate::auth::epoch_guard;
use crate::auth::registry::{key_hex, GroupKeyRegistry, UnlockedKey};
use crate::auth::subkeys::encode_hex;
use crate::persistence::{migrations, personal_store};
use crate::provider_credentials::session::ConnectSessions;
use crate::test_support::ENV_MUTEX;

const USER: &str = "rot-user";
const PASSWORD: &[u8] = b"correct horse";
const NEW_PASSWORD: &[u8] = b"battery staple";
const PERSONA: &str = "persona-a";
const SMALL_KDF: KdfParams = KdfParams {
    memory_kib: 1024,
    iterations: 1,
    parallelism: 1,
};

/// A file the fixture created and what it holds, for before/after checks.
#[derive(Clone)]
struct Expected {
    rel_path: String,
    /// For a database, the marker row text. For a blob, its plaintext.
    content: Vec<u8>,
}

struct Fixture {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    _tempdir: tempfile::TempDir,
    saved_root: Option<String>,
    pool: sqlx::SqlitePool,
    user_dir: PathBuf,
    dek: SecretBytes,
    recovery_entropy: SecretBytes,
    expected: Vec<Expected>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        epoch_guard::clear_resident_for(USER);
        match &self.saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

async fn populate_registry(registry: &KeyRegistry, user: &str, dek: [u8; 32], epoch: u64) {
    let (sharing_private, _) = sharing_keypair::derive_sharing_keypair(&dek, user, epoch);
    registry
        .replace(UnlockedKey {
            user_id: user.to_owned(),
            data_key: dek,
            key_epoch: epoch,
            sharing_private_key: sharing_private.to_bytes(),
            unlocked_at: crate::providers::utils::now(),
        })
        .await;
}

async fn mark_db(path: &Path, key: &str, text: &str) {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key}'\""))
        .pragma("cipher_compatibility", "4")
        .connect()
        .await
        .unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS qr_marker (v TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO qr_marker (v) VALUES (?)")
        .bind(text)
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
}

async fn read_marker(path: &Path, key: &str) -> Option<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key}'\""))
        .pragma("cipher_compatibility", "4")
        .connect()
        .await
        .ok()?;
    let v = sqlx::query_scalar("SELECT v FROM qr_marker LIMIT 1")
        .fetch_one(&mut conn)
        .await
        .ok();
    let _ = conn.close().await;
    v
}

impl Fixture {
    async fn new() -> Fixture {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        migrations::migrate_shared_db().await.unwrap();
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .unwrap();

        // The account: real wrapper at epoch 1 under PASSWORD.
        let salt = kdf::generate_salt().unwrap();
        let pk = kdf::derive_password_key(PASSWORD, &salt, 1024, 1, 1).unwrap();
        let material = data_key::create_account_key_material(USER, &pk).unwrap();
        let (_, sharing_pub) = sharing_keypair::derive_sharing_keypair(&material.dek, USER, 1);
        user_store::create_user_with_wrapper(
            &pool,
            USER,
            "Rotator",
            "admin",
            true,
            &salt,
            1024,
            1,
            1,
            sharing_pub.as_bytes(),
            &material.wrapper,
        )
        .await
        .unwrap();
        epoch_guard::set_resident(USER, 1);

        let user_dir = tempdir.path().join("users").join(USER);
        let mut fx = Fixture {
            _lock: lock,
            _tempdir: tempdir,
            saved_root,
            pool,
            user_dir,
            dek: material.dek.clone(),
            recovery_entropy: material.recovery_entropy.clone(),
            expected: Vec::new(),
        };
        fx.populate().await;
        fx
    }

    /// Real databases of every kind and some blobs, all under the epoch-1 DEK.
    async fn populate(&mut self) {
        let hex = key_hex(&self.dek);
        migrations::migrate_keys_db(USER, &hex).await.unwrap();
        migrations::migrate_tier3_cookies_db(USER, &hex)
            .await
            .unwrap();
        migrations::migrate_personal_db(USER, PERSONA, &hex)
            .await
            .unwrap();
        migrations::migrate_outputs_db(USER, PERSONA, &hex)
            .await
            .unwrap();
        migrations::migrate_messages_db(USER, PERSONA, &hex)
            .await
            .unwrap();
        migrations::migrate_domain_context_db(USER, PERSONA, "focus-1", &hex)
            .await
            .unwrap();
        migrations::migrate_plan_state_db(USER, PERSONA, "focus-1", "topic-1", &hex)
            .await
            .unwrap();
        migrations::migrate_view_cache_db(USER, "share-1", &hex)
            .await
            .unwrap();

        let inv = io::inventory(&self.user_dir).await.unwrap();
        for item in &inv.items {
            if let ItemTarget::Db(t) = &item.target {
                let key = io::db_key_for(t, &self.dek, USER, 1).unwrap();
                let text = format!("marker:{}", item.rel_path);
                mark_db(&item.abs_path, &key, &text).await;
                self.expected.push(Expected {
                    rel_path: item.rel_path.clone(),
                    content: text.into_bytes(),
                });
            }
        }

        for (output, version, text) in [
            ("out-1", 1, "first document"),
            ("out-1", 2, "second version, retained"),
            ("out-2", 1, "another document"),
        ] {
            let path = ingest_blob::ingest_blob_path(USER, PERSONA, output, version);
            ingest_blob::write_encrypted_blob(
                &path,
                &self.dek,
                USER,
                PERSONA,
                output,
                version,
                text.as_bytes(),
            )
            .await
            .unwrap();
            self.expected.push(Expected {
                rel_path: format!("personas/{PERSONA}/ingested_documents/{output}/v{version}.enc"),
                content: text.as_bytes().to_vec(),
            });
        }
        // A stale .tmp from an earlier interrupted write: old-key ciphertext.
        let stale =
            ingest_blob::stale_tmp_path(&ingest_blob::ingest_blob_path(USER, PERSONA, "out-1", 1));
        std::fs::write(stale, b"stale old ciphertext").unwrap();
    }

    fn env(&self) -> RotationEnv<'_> {
        let mut env = RotationEnv::new(&self.pool, USER, self.user_dir.clone());
        env.kdf = SMALL_KDF;
        env
    }

    async fn rotate(&self, request: RotationRequest) -> Result<RotationOutcome, RotationError> {
        run_rotation(&self.env(), &self.dek, 1, request).await
    }

    async fn rotate_with_fault(
        &self,
        fault: FaultPoint,
        request: RotationRequest,
    ) -> Result<RotationOutcome, RotationError> {
        let mut env = self.env();
        env.faults = vec![fault];
        run_rotation(&env, &self.dek, 1, request).await
    }

    fn change_password() -> RotationRequest {
        RotationRequest::PasswordChange {
            new_password: Zeroizing::new(NEW_PASSWORD.to_vec()),
        }
    }

    async fn wrapper(&self) -> user_store::WrapperState {
        user_store::get_key_wrapper_state(&self.pool, USER)
            .await
            .unwrap()
            .unwrap()
    }

    /// The DEK a login with `password` would get from the LIVE wrapper.
    async fn unlock_live(&self, password: &[u8]) -> Result<SecretBytes, DataKeyError> {
        let state = self.wrapper().await;
        let (salt, m, i, p) = user_store::get_salt_params(&self.pool, USER)
            .await
            .unwrap()
            .unwrap();
        let pk = kdf::derive_password_key(password, &salt, m, i, p).unwrap();
        data_key::unwrap_dek(
            &pk,
            &state.wrapper.password_wrap,
            USER,
            state.wrapper.key_epoch,
            state.wrapper.format_version,
        )
    }

    /// Every recorded file opens under `dek` at `epoch` with its content.
    async fn assert_all_under(&self, dek: &[u8; 32], epoch: u64) {
        for e in &self.expected {
            let abs = self.user_dir.join(&e.rel_path);
            match io::target_for_rel_path(&e.rel_path).unwrap() {
                ItemTarget::Db(t) => {
                    let key = io::db_key_for(&t, dek, USER, epoch).unwrap();
                    assert_eq!(
                        read_marker(&abs, &key).await.as_deref().map(str::as_bytes),
                        Some(e.content.as_slice()),
                        "{} must open under epoch {epoch} with its content",
                        e.rel_path
                    );
                    io::verify_db_key(&abs, &key).await.unwrap();
                }
                ItemTarget::Blob(b) => {
                    assert!(
                        ingest_blob::blob_opens_under(
                            &abs,
                            USER,
                            &b.persona_id,
                            &b.output_id,
                            b.version,
                            dek,
                            epoch
                        )
                        .await
                        .unwrap(),
                        "{} must open under epoch {epoch}",
                        e.rel_path
                    );
                }
            }
        }
    }

    /// No recorded file opens under `dek` at `epoch`.
    async fn assert_none_under(&self, dek: &[u8; 32], epoch: u64) {
        for e in &self.expected {
            let abs = self.user_dir.join(&e.rel_path);
            match io::target_for_rel_path(&e.rel_path).unwrap() {
                ItemTarget::Db(t) => {
                    let key = io::db_key_for(&t, dek, USER, epoch).unwrap();
                    assert!(
                        !io::db_opens_under(&abs, &key).await,
                        "{} must NOT open under epoch {epoch}",
                        e.rel_path
                    );
                }
                ItemTarget::Blob(b) => {
                    assert!(
                        !ingest_blob::blob_opens_under(
                            &abs,
                            USER,
                            &b.persona_id,
                            &b.output_id,
                            b.version,
                            dek,
                            epoch
                        )
                        .await
                        .unwrap(),
                        "{} must NOT open under epoch {epoch}",
                        e.rel_path
                    );
                }
            }
        }
    }

    async fn assert_no_leftovers(&self) {
        let inv = io::inventory(&self.user_dir).await.unwrap();
        assert!(inv.sidecars.is_empty(), "sidecars: {:?}", inv.sidecars);
        assert!(inv.stale_tmp.is_empty(), "stale tmp: {:?}", inv.stale_tmp);
    }

    /// After a refusal nothing was cleaned up either: only the .tmp the
    /// fixture planted may remain.
    async fn assert_no_leftovers_except_the_planted_tmp(&self) {
        let inv = io::inventory(&self.user_dir).await.unwrap();
        assert!(inv.sidecars.is_empty(), "sidecars: {:?}", inv.sidecars);
        assert_eq!(inv.stale_tmp.len(), 1, "the planted .tmp is still there");
    }

    /// Resume the pending rotation the way an unlock with the NEW password
    /// would.
    async fn resume_with_new_password(&self) -> Result<ResumeOutcome, RotationError> {
        let pending = user_store::get_pending_rotation(&self.pool, USER)
            .await
            .unwrap()
            .expect("a rotation must be pending");
        let pk = kdf::derive_password_key(
            NEW_PASSWORD,
            pending.salt.as_ref().unwrap(),
            pending.kdf_memory_kib.unwrap(),
            pending.kdf_iterations.unwrap(),
            pending.kdf_parallelism.unwrap(),
        )
        .unwrap();
        let new_dek = new_dek_from_staged_password(&pending, USER, &pk)?;
        resume_rotation(&self.env(), &pending, new_dek, Credential::Password(&pk)).await
    }
}

/// The whole account after a completed rotation to epoch 2 under NEW_PASSWORD.
async fn assert_completed(fx: &Fixture, new_dek: &[u8; 32]) {
    let state = fx.wrapper().await;
    assert_eq!(state.wrapper.key_epoch, 2);
    assert_eq!(state.pending_rotation, None);
    assert!(user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .is_none());
    let live = fx.unlock_live(NEW_PASSWORD).await.unwrap();
    assert_eq!(*live, *new_dek);
    assert!(
        fx.unlock_live(PASSWORD).await.is_err(),
        "the old password must open nothing current"
    );
    fx.assert_all_under(new_dek, 2).await;
    fx.assert_none_under(&fx.dek, 1).await;
    fx.assert_no_leftovers().await;
    assert!(!epoch_guard::is_rotating());
}

// ---------------------------------------------------------------------------
// The three rotations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn password_change_rekeys_everything_and_the_old_password_opens_nothing() {
    let fx = Fixture::new().await;
    assert!(fx.expected.len() >= 11, "fixture covers every kind of file");
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();

    assert_eq!((outcome.from_epoch, outcome.to_epoch), (1, 2));
    assert_eq!(outcome.kind, RotationKind::PasswordChange);
    assert_ne!(*outcome.new_dek, *fx.dek, "a NEW random data key");
    assert_completed(&fx, &outcome.new_dek).await;

    // The committed rotation keeps no secrets.
    let done = user_store::get_rotation(&fx.pool, &outcome.rotation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, RotationState::Committed);
    assert!(done.old_dek_under_new.is_none() && done.password_wrap.is_none());
    assert!(done.recovery_seal.is_none() && done.salt.is_none());

    // The sharing keypair moved to the new epoch and was published.
    let (_, expected_public) = sharing_keypair::derive_sharing_keypair(&outcome.new_dek, USER, 2);
    assert_eq!(
        sharing_keypair::get_public_key(&fx.pool, USER)
            .await
            .unwrap(),
        Some(*expected_public.as_bytes())
    );
    assert_eq!(outcome.new_sharing_public_key, *expected_public.as_bytes());
    assert_ne!(
        outcome.old_sharing_public_key,
        Some(outcome.new_sharing_public_key)
    );
    let (epoch,): (i64,) =
        sqlx::query_as("SELECT key_epoch FROM user_sharing_keys WHERE user_id = ?")
            .bind(USER)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(epoch, 2);

    // The ordinary stores open the rotated account once the new epoch is
    // registered, and the old DEK no longer opens them.
    epoch_guard::set_resident(USER, 2);
    let new_hex = key_hex(&outcome.new_dek);
    assert!(personal_store::open_personal_db(USER, PERSONA, &new_hex)
        .await
        .is_ok());
    assert!(
        personal_store::open_personal_db(USER, PERSONA, &key_hex(&fx.dek))
            .await
            .is_err()
    );
    let blob = ingest_blob::ingest_blob_path(USER, PERSONA, "out-1", 2);
    assert_eq!(
        ingest_blob::read_encrypted_blob(&blob, &outcome.new_dek, USER, PERSONA, "out-1", 2)
            .await
            .unwrap(),
        b"second version, retained"
    );
}

#[tokio::test]
async fn the_recovery_phrase_survives_a_password_change() {
    let fx = Fixture::new().await;
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();

    // The OLD phrase still unseals the NEW epoch's seal.
    let state = fx.wrapper().await;
    let (secret, public) = data_key::derive_recovery_keypair(&fx.recovery_entropy, USER);
    assert_eq!(public.as_bytes(), &state.wrapper.recovery_public_key);
    let unsealed = data_key::unseal_dek_with_recovery_key(
        &secret,
        &state.wrapper.recovery_seal,
        USER,
        state.wrapper.key_epoch,
        state.wrapper.format_version,
    )
    .unwrap();
    assert_eq!(*unsealed, *outcome.new_dek);
}

#[tokio::test]
async fn a_new_phrase_revokes_the_old_words_and_keeps_the_password() {
    let fx = Fixture::new().await;
    let new_entropy = data_key::generate_recovery_entropy().unwrap();
    let outcome = fx
        .rotate(RotationRequest::NewPhrase {
            password: Zeroizing::new(PASSWORD.to_vec()),
            recovery_entropy: new_entropy.clone(),
        })
        .await
        .unwrap();
    assert_eq!(outcome.kind, RotationKind::NewPhrase);

    // Same password, new key.
    let live = fx.unlock_live(PASSWORD).await.unwrap();
    assert_eq!(*live, *outcome.new_dek);
    fx.assert_all_under(&outcome.new_dek, 2).await;
    fx.assert_none_under(&fx.dek, 1).await;
    fx.assert_no_leftovers().await;

    let state = fx.wrapper().await;
    let w = &state.wrapper;
    // The old words derive a different public key than the stored one, and
    // cannot unseal the new seal.
    let (old_secret, old_public) = data_key::derive_recovery_keypair(&fx.recovery_entropy, USER);
    assert_ne!(old_public.as_bytes(), &w.recovery_public_key);
    assert!(data_key::unseal_dek_with_recovery_key(
        &old_secret,
        &w.recovery_seal,
        USER,
        w.key_epoch,
        w.format_version
    )
    .is_err());
    // The new words do.
    let (new_secret, new_public) = data_key::derive_recovery_keypair(&new_entropy, USER);
    assert_eq!(new_public.as_bytes(), &w.recovery_public_key);
    let unsealed = data_key::unseal_dek_with_recovery_key(
        &new_secret,
        &w.recovery_seal,
        USER,
        w.key_epoch,
        w.format_version,
    )
    .unwrap();
    assert_eq!(*unsealed, *outcome.new_dek);
}

#[tokio::test]
async fn restore_rotates_exactly_like_a_password_change() {
    let fx = Fixture::new().await;
    // The caller recovered the old DEK by unsealing with the typed words.
    let state = fx.wrapper().await;
    let (secret, _) = data_key::derive_recovery_keypair(&fx.recovery_entropy, USER);
    let recovered = data_key::unseal_dek_with_recovery_key(
        &secret,
        &state.wrapper.recovery_seal,
        USER,
        1,
        state.wrapper.format_version,
    )
    .unwrap();
    let outcome = run_rotation(
        &fx.env(),
        &recovered,
        1,
        RotationRequest::Restore {
            new_password: Zeroizing::new(NEW_PASSWORD.to_vec()),
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.kind, RotationKind::Restore);
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn a_forged_recovered_key_cannot_start_a_restore() {
    let fx = Fixture::new().await;
    // A forged seal yields some key that is not the account's.
    let forged = data_key::generate_dek().unwrap();
    let err = run_rotation(
        &fx.env(),
        &forged,
        1,
        RotationRequest::Restore {
            new_password: Zeroizing::new(NEW_PASSWORD.to_vec()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, RotationError::DatabaseUnreadable { .. }),
        "{err:?}"
    );
    // Nothing changed.
    assert_eq!(fx.wrapper().await.pending_rotation, None);
    fx.assert_all_under(&fx.dek, 1).await;
    assert!(!epoch_guard::is_rotating());
}

#[tokio::test]
async fn a_completed_rotation_ends_the_session_and_clears_the_registry() {
    // items.id=778: after a rotation nothing is resident. The registry's key,
    // the resident epoch and the pending slots are gone, and the gate is idle.
    let fx = Fixture::new().await;
    let registry = KeyRegistry::default();
    populate_registry(&registry, USER, *fx.dek, 1).await;
    registry
        .set_pending_new_phrase(crate::auth::registry::NewPhraseSlot {
            entropy: data_key::generate_recovery_entropy().unwrap(),
            confirm_positions: vec![1, 2, 3],
            failed_confirmations: 0,
        })
        .await;
    let mut env = fx.env();
    env.registry = Some(&registry);
    run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();

    assert!(!registry.is_occupied().await);
    assert!(registry.with_key(|k| k.key_epoch).await.is_none());
    assert!(registry.with_pending_new_phrase(|_| ()).await.is_none());
    assert_eq!(epoch_guard::resident_epoch(USER), None);
    assert!(crate::auth::writer_gate::is_idle(USER));
    assert!(!epoch_guard::is_rotating_for(USER), "the flag is released");
}

#[tokio::test]
async fn the_sharing_hook_runs_while_the_flag_is_held_and_before_the_registry_is_cleared() {
    let fx = Fixture::new().await;
    let registry = KeyRegistry::default();
    populate_registry(&registry, USER, *fx.dek, 1).await;
    *HOOK_OBSERVATION.lock().unwrap() = None;
    let mut env = fx.env();
    env.registry = Some(&registry);
    run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();
    assert_eq!(
        *HOOK_OBSERVATION.lock().unwrap(),
        Some(HookObservation {
            rotating: true,
            registry_occupied: true
        }),
        "the hook sees the rotating flag and the (old) key still resident"
    );
    assert!(
        !registry.is_occupied().await,
        "and the clear comes after it"
    );
}

#[tokio::test]
async fn a_failure_after_promotion_ends_the_session() {
    // After promotion the data is on the new key: the resident (old) key is
    // stale, so the session ends (items.id=778 amendment).
    let fx = Fixture::new().await;
    let registry = KeyRegistry::default();
    populate_registry(&registry, USER, *fx.dek, 1).await;
    let mut env = fx.env();
    env.registry = Some(&registry);
    env.faults = vec![FaultPoint::FailAfterPromote];
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::NeedsResume { .. }), "{err:?}");
    assert!(!registry.is_occupied().await);
}

#[tokio::test]
async fn a_rollback_before_promotion_keeps_the_session() {
    let fx = Fixture::new().await;
    let registry = KeyRegistry::default();
    populate_registry(&registry, USER, *fx.dek, 1).await;
    let mut env = fx.env();
    env.registry = Some(&registry);
    env.faults = vec![FaultPoint::FailAfterItemCommitted(0)];
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::RolledBack { .. }), "{err:?}");
    assert!(registry.is_occupied().await, "a rollback changes nothing");
    assert_eq!(registry.with_key(|k| k.key_epoch).await, Some(1));
}

// ---------------------------------------------------------------------------
// A second session locks on the epoch change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_session_locks_when_the_epoch_changes() {
    let fx = Fixture::new().await;
    // This process stands in for the OTHER session: it holds the old key.
    let other = KeyRegistry::default();
    populate_registry(&other, USER, *fx.dek, 1).await;

    // The rotation is done by "another process": no registry swap here.
    fx.rotate(Fixture::change_password()).await.unwrap();

    assert!(matches!(
        epoch_guard::ensure_current(USER).await,
        Err(epoch_guard::KeyEpochError::Changed {
            held: 1,
            current: Some(2)
        })
    ));
    // A store open refuses with the epoch error, not a confusing decrypt one.
    assert!(matches!(
        personal_store::open_personal_db(USER, PERSONA, &key_hex(&fx.dek)).await,
        Err(personal_store::PersonalStoreError::KeyEpoch(_))
    ));
    // And the periodic check locks the session.
    idle_timeout_check(&fx, &other).await;
    assert!(
        !other.is_occupied().await,
        "the stale session must be locked"
    );
}

async fn idle_timeout_check(fx: &Fixture, registry: &KeyRegistry) {
    crate::auth::idle_timeout::run_periodic_check(
        &fx.pool,
        registry,
        &GroupKeyRegistry::default(),
        &ConnectSessions::default(),
    )
    .await;
}

#[tokio::test]
async fn background_checks_stand_down_while_a_rotation_runs() {
    let fx = Fixture::new().await;
    // A session whose key would normally be locked (epoch mismatch).
    let session = KeyRegistry::default();
    populate_registry(&session, USER, *fx.dek, 1).await;
    fx.rotate(Fixture::change_password()).await.unwrap();

    let guard = epoch_guard::begin_rotation(USER).unwrap();
    idle_timeout_check(&fx, &session).await;
    assert!(
        session.is_occupied().await,
        "no lock decision is taken mid-rotation"
    );
    drop(guard);
    idle_timeout_check(&fx, &session).await;
    assert!(!session.is_occupied().await);
}

#[tokio::test]
async fn the_idle_check_stands_down_while_the_writer_gate_drains() {
    // items.id=778: a drain can wait minutes for a reply to finish; the
    // periodic check must not lock the session out from under it.
    let fx = Fixture::new().await;
    let session = KeyRegistry::default();
    populate_registry(&session, USER, *fx.dek, 1).await;
    fx.rotate(Fixture::change_password()).await.unwrap();

    let drain = crate::auth::writer_gate::begin_drain(USER).unwrap();
    idle_timeout_check(&fx, &session).await;
    assert!(
        session.is_occupied().await,
        "no lock decision is taken while the gate drains"
    );
    drop(drain);
    idle_timeout_check(&fx, &session).await;
    assert!(!session.is_occupied().await);
}

// ---------------------------------------------------------------------------
// Progress and the rotating flag
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<(ProgressEvent, bool)>>,
}

impl ProgressSink for Recorder {
    fn emit(&self, event: &ProgressEvent) {
        self.events
            .lock()
            .unwrap()
            .push((event.clone(), epoch_guard::is_rotating_for(USER)));
    }
}

#[tokio::test]
async fn progress_is_reported_in_order_in_bytes_with_no_paths_and_the_flag_held() {
    let fx = Fixture::new().await;
    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let outcome = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();

    let events = recorder.events.lock().unwrap();
    let phases: Vec<Phase> = events.iter().map(|(e, _)| e.phase).collect();
    assert_eq!(phases.first(), Some(&Phase::Preparing));
    assert_eq!(phases.last(), Some(&Phase::Complete));
    let rekeying = phases.iter().filter(|p| **p == Phase::Rekeying).count();
    assert_eq!(rekeying, fx.expected.len());
    let verifying = phases.iter().position(|p| *p == Phase::Verifying).unwrap();
    let finishing = phases.iter().position(|p| *p == Phase::Finishing).unwrap();
    assert!(verifying < finishing);
    assert!(phases[..verifying].iter().all(|p| *p != Phase::Complete));

    // Preparing comes before the account is inventoried, so it has no counts
    // (items.id=780); every later event counts bytes against the same total.
    assert_eq!((events[0].0.current, events[0].0.total), (None, None));
    let total = events[1].0.total.unwrap();
    assert!(total > 0);
    let mut last = 0;
    for (e, _) in events.iter().skip(1) {
        assert_eq!(e.total, Some(total));
        let current = e.current.unwrap();
        assert!(current >= last && current <= total);
        last = current;
    }
    for (e, rotating) in events.iter() {
        assert_eq!(e.rotation_id, outcome.rotation_id);
        assert!(*rotating, "every event is emitted with the flag held");
        if let Some(m) = &e.message {
            assert!(
                !m.contains(PERSONA) && !m.contains('/') && !m.contains(".db"),
                "labels carry no paths or ids: {m}"
            );
        }
    }
    assert_eq!(events.last().unwrap().0.current, Some(total));
}

#[tokio::test]
async fn preparing_is_reported_before_preflight_and_a_refusal_closes_it() {
    // items.id=780: the overlay must be up for the whole of preflight, so
    // Preparing is emitted before it. Proof: a refusal that preflight alone
    // produces (no room for the largest file) still leaves a Preparing event
    // ahead of it, and NotStarted closes the report with nothing staged.
    let fx = Fixture::new().await;
    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    env.free_space = |_| Some(10);
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(
        matches!(err, RotationError::InsufficientSpace { .. }),
        "{err:?}"
    );

    {
        let events = recorder.events.lock().unwrap();
        let phases: Vec<Phase> = events.iter().map(|(e, _)| e.phase).collect();
        assert_eq!(phases, vec![Phase::Preparing, Phase::NotStarted]);
        for (e, rotating) in events.iter() {
            assert_eq!((e.current, e.total), (None, None));
            assert!(*rotating, "both are emitted with the flag held");
            assert_eq!(e.rotation_id, events[0].0.rotation_id);
        }
    }
    assert!(
        user_store::get_key_wrapper_state(&fx.pool, USER)
            .await
            .unwrap()
            .unwrap()
            .pending_rotation
            .is_none(),
        "nothing was staged"
    );
    fx.assert_all_under(&fx.dek, 1).await;
}

#[test]
fn only_a_large_database_is_an_opaque_step() {
    let item = |is_blob: bool, bytes: u64| RotationItem {
        rel_path: "x".to_owned(),
        is_blob,
        state: RotationState::Pending,
        bytes,
        error: None,
    };
    assert!(is_opaque_step(&item(false, OPAQUE_STEP_MIN_BYTES)));
    assert!(!is_opaque_step(&item(false, OPAQUE_STEP_MIN_BYTES - 1)));
    assert!(!is_opaque_step(&item(true, OPAQUE_STEP_MIN_BYTES * 8)));
}

#[tokio::test]
async fn stores_and_a_second_rotation_are_refused_while_one_runs() {
    let fx = Fixture::new().await;
    let guard = epoch_guard::begin_rotation(USER).unwrap();
    assert!(matches!(
        personal_store::open_personal_db(USER, PERSONA, &key_hex(&fx.dek)).await,
        Err(personal_store::PersonalStoreError::KeyEpoch(
            epoch_guard::KeyEpochError::Rotating
        ))
    ));
    let err = fx.rotate(Fixture::change_password()).await.unwrap_err();
    assert!(matches!(err, RotationError::RotationInProgress), "{err:?}");
    drop(guard);
    fx.assert_all_under(&fx.dek, 1).await;
}

// ---------------------------------------------------------------------------
// Preflight: refusals leave the account untouched
// ---------------------------------------------------------------------------

async fn assert_untouched(fx: &Fixture) {
    assert_eq!(fx.wrapper().await.pending_rotation, None);
    assert_eq!(fx.wrapper().await.wrapper.key_epoch, 1);
    assert!(user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .is_none());
    fx.assert_all_under(&fx.dek, 1).await;
    assert!(!epoch_guard::is_rotating());
}

#[tokio::test]
async fn an_unrecognised_encrypted_file_stops_the_rotation_before_anything_changes() {
    let fx = Fixture::new().await;
    std::fs::write(fx.user_dir.join("personas/persona-a/mystery.db"), b"?").unwrap();
    let err = fx.rotate(Fixture::change_password()).await.unwrap_err();
    assert!(
        matches!(&err, RotationError::UnknownEncryptedFile(p) if p.ends_with("mystery.db")),
        "{err:?}"
    );
    assert_untouched(&fx).await;
}

#[tokio::test]
async fn a_rotation_that_requires_a_closed_gate_waits_for_the_writers() {
    use crate::auth::writer_gate::{self, Kind};
    let fx = Fixture::new().await;
    let mut env = fx.env();
    env.require_closed_gate = true;

    // The caller forgot to drain: refused, nothing changed.
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::GateNotClosed), "{err:?}");
    assert_untouched(&fx).await;

    // Drained and closed, but a writer still holds a lease: refused with the
    // count, nothing changed.
    let lease = writer_gate::acquire(USER, Kind::Command).unwrap();
    let drain = writer_gate::begin_drain(USER).unwrap();
    assert!(drain
        .close(std::time::Duration::from_millis(20))
        .await
        .is_err());
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::ActiveRuns(1)), "{err:?}");
    assert_untouched(&fx).await;

    // Writer done, gate closed and empty: the rotation runs.
    drop(lease);
    drain
        .close(std::time::Duration::from_millis(20))
        .await
        .unwrap();
    run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();
    drop(drain);
    assert!(writer_gate::is_idle(USER));
}

#[tokio::test]
async fn too_little_disk_space_blocks_the_rotation() {
    let fx = Fixture::new().await;
    let mut env = fx.env();
    env.free_space = |_| Some(10);
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(
        matches!(err, RotationError::InsufficientSpace { .. }),
        "{err:?}"
    );
    assert_untouched(&fx).await;

    env.free_space = |_| None;
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::FreeSpaceUnknown), "{err:?}");
    assert_untouched(&fx).await;
}

#[tokio::test]
async fn a_stale_epoch_is_refused() {
    let fx = Fixture::new().await;
    let err = run_rotation(&fx.env(), &fx.dek, 5, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::EpochChanged), "{err:?}");
    assert_untouched(&fx).await;
}

#[tokio::test]
async fn a_restore_needs_the_database_that_proves_the_key() {
    let fx = Fixture::new().await;
    std::fs::remove_file(fx.user_dir.join("integration_keys.db")).unwrap();
    let err = run_rotation(
        &fx.env(),
        &fx.dek,
        1,
        RotationRequest::Restore {
            new_password: Zeroizing::new(NEW_PASSWORD.to_vec()),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RotationError::NoProofDatabase), "{err:?}");
}

#[tokio::test]
async fn a_database_held_by_something_else_stops_the_rotation_untouched() {
    let fx = Fixture::new().await;
    // Another connection takes personal.db's exclusive lock and keeps it.
    let personal_rel = "personas/persona-a/personal.db";
    let target = match io::target_for_rel_path(personal_rel).unwrap() {
        ItemTarget::Db(t) => t,
        _ => unreachable!(),
    };
    let key = io::db_key_for(&target, &fx.dek, USER, 1).unwrap();
    let mut holder = SqliteConnectOptions::new()
        .filename(fx.user_dir.join(personal_rel))
        .create_if_missing(false)
        .pragma("key", format!("\"x'{}'\"", key.as_str()))
        .pragma("cipher_compatibility", "4")
        .pragma("locking_mode", "EXCLUSIVE")
        .connect()
        .await
        .unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut holder)
        .await
        .unwrap();

    let err = fx.rotate(Fixture::change_password()).await.unwrap_err();
    assert!(
        matches!(&err, RotationError::Busy { rel_path } if rel_path == personal_rel),
        "{err:?}"
    );
    holder.close().await.unwrap();
    // Nothing was staged, moved or deleted.
    assert_untouched(&fx).await;
    fx.assert_no_leftovers_except_the_planted_tmp().await;
    // Once the other use ends, the same rotation goes through.
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();
    assert_completed(&fx, &outcome.new_dek).await;
}

// ---------------------------------------------------------------------------
// Bounded retry of transient failures (items.id=778, decisions.id=896 point 7)
// ---------------------------------------------------------------------------

fn no_wait() -> [std::time::Duration; REKEY_ATTEMPTS - 1] {
    [std::time::Duration::ZERO; REKEY_ATTEMPTS - 1]
}

#[test]
fn only_a_busy_file_or_a_stuck_sidecar_is_transient() {
    assert!(RotationError::Busy {
        rel_path: "x".into()
    }
    .is_transient());
    assert!(RotationError::LeftoverSidecar {
        rel_path: "x".into()
    }
    .is_transient());
    assert!(RotationError::Io(std::io::Error::from(std::io::ErrorKind::WouldBlock)).is_transient());
    assert!(!RotationError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)).is_transient());
    for hard in [
        RotationError::EpochChanged,
        RotationError::Item {
            rel_path: "x".into(),
            detail: "y".into(),
        },
        RotationError::DatabaseUnreadable {
            rel_path: "x".into(),
            detail: "y".into(),
        },
        RotationError::ActiveRuns(1),
    ] {
        assert!(!hard.is_transient(), "{hard:?}");
    }
}

#[tokio::test]
async fn a_transient_failure_is_retried_and_the_rotation_completes() {
    let fx = Fixture::new().await;
    let mut env = fx.env();
    env.retry_backoff = no_wait();
    // Item 1 is busy on its first four attempts and fine on the fifth.
    env.transient_failures = Some((1, std::sync::atomic::AtomicUsize::new(REKEY_ATTEMPTS - 1)));
    let outcome = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn a_failure_that_outlasts_the_retries_rolls_back_and_changes_nothing() {
    let fx = Fixture::new().await;
    let mut env = fx.env();
    env.retry_backoff = no_wait();
    // Busy on every attempt: five tries, then the ordinary rollback.
    env.transient_failures = Some((1, std::sync::atomic::AtomicUsize::new(REKEY_ATTEMPTS)));
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::RolledBack { .. }), "{err:?}");
    // The failed attempt used exactly the budget, no more.
    let (_, left) = env.transient_failures.as_ref().unwrap();
    assert_eq!(left.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn a_retry_stops_when_the_rotation_is_no_longer_ours() {
    // Between attempts the engine re-checks the wrapper; a changed epoch ends
    // the retries with the epoch error, not another attempt.
    let fx = Fixture::new().await;
    let mut env = fx.env();
    env.retry_backoff = no_wait();
    env.transient_failures = Some((1, std::sync::atomic::AtomicUsize::new(REKEY_ATTEMPTS)));
    // Another process changes the account's epoch right after staging.
    sqlx::query("UPDATE user_key_wrappers SET key_epoch = key_epoch + 1 WHERE user_id = ?")
        .bind(USER)
        .execute(&fx.pool)
        .await
        .unwrap();
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::EpochChanged), "{err:?}");
}

#[tokio::test]
async fn a_file_that_appears_during_the_rotation_fails_into_the_rollback_path() {
    let fx = Fixture::new().await;
    let intruder = fx.user_dir.join(format!(
        "personas/{PERSONA}/ingested_documents/out-3/v1.enc"
    ));
    // Planted while the last item is being re-keyed, so the post-items
    // inventory sees it.
    struct Plant {
        path: PathBuf,
        done: std::sync::atomic::AtomicBool,
    }
    impl ProgressSink for Plant {
        fn emit(&self, event: &ProgressEvent) {
            if event.phase == Phase::Rekeying
                && event.message.as_deref() == Some("Documents")
                && !self.done.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                std::fs::create_dir_all(self.path.parent().unwrap()).unwrap();
                std::fs::write(&self.path, b"written by something else").unwrap();
            }
        }
    }
    let sink = Plant {
        path: intruder.clone(),
        done: std::sync::atomic::AtomicBool::new(false),
    };
    let mut env = fx.env();
    env.progress = &sink;
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, RotationError::RolledBack { cause } if cause.contains("appeared")),
        "{err:?}"
    );

    // Rolled back: the live wraps and every staged file are as before, and the
    // intruder was neither touched nor counted.
    assert_eq!(fx.wrapper().await.pending_rotation, None);
    assert_eq!(fx.wrapper().await.wrapper.key_epoch, 1);
    fx.assert_all_under(&fx.dek, 1).await;
    assert_eq!(
        std::fs::read(&intruder).unwrap(),
        b"written by something else"
    );
}

// ---------------------------------------------------------------------------
// Interrupted rotation leaves a usable account at EACH point (crash table)
// ---------------------------------------------------------------------------

async fn crash_then_resume_forward(fault: FaultPoint) {
    let fx = Fixture::new().await;
    let err = fx
        .rotate_with_fault(fault, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(
        matches!(err, RotationError::SimulatedCrash),
        "{fault:?}: {err:?}"
    );
    assert!(
        !epoch_guard::is_rotating(),
        "a dead process holds no flag: {fault:?}"
    );
    let state = fx.wrapper().await;
    assert!(state.pending_rotation.is_some(), "{fault:?}: still pending");

    // The old password alone cannot resume: it reaches only the old wrap (or,
    // after promotion, nothing).
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    if pending.state != RotationState::Verifying {
        let live_old = fx.unlock_live(PASSWORD).await.unwrap();
        assert_eq!(
            *live_old, *fx.dek,
            "the old password still opens the old wrap"
        );
        let pk_old = kdf::derive_password_key(
            PASSWORD,
            pending.salt.as_ref().unwrap(),
            pending.kdf_memory_kib.unwrap(),
            pending.kdf_iterations.unwrap(),
            pending.kdf_parallelism.unwrap(),
        )
        .unwrap();
        assert!(new_dek_from_staged_password(&pending, USER, &pk_old).is_err());
    }

    let resumed = fx.resume_with_new_password().await.unwrap();
    let ResumeOutcome::Completed(outcome) = resumed else {
        panic!("{fault:?}: expected the rotation to complete");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

// ---------------------------------------------------------------------------
// The kill matrix with a concurrent writer (items.id=778, decisions.id=896
// point 9): a reply is writing while the user asks for a rotation, the
// rotation waits for it (finished or stopped), then dies at each fault point
// and resumes forward. What the writer saved must survive under the new key.
// ---------------------------------------------------------------------------

async fn read_markers(path: &Path, key: &str) -> Vec<String> {
    let mut conn = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key}'\""))
        .pragma("cipher_compatibility", "4")
        .connect()
        .await
        .unwrap();
    let rows = sqlx::query_scalar("SELECT v FROM qr_marker")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    rows
}

/// Which way the concurrent writer ends.
#[derive(Clone, Copy, Debug)]
enum WriterEnds {
    /// 'Finish first': it finishes on its own.
    Finishes,
    /// 'Stop and save': it is told to stop, saves what it has, then ends.
    IsStopped,
}

async fn crash_with_a_concurrent_writer_then_resume(fault: FaultPoint, ends: WriterEnds) {
    use crate::auth::writer_gate::{self, Choice, DrainPolicy, Kind};

    let fx = Fixture::new().await;
    let inv = io::inventory(&fx.user_dir).await.unwrap();
    let personal = inv
        .items
        .iter()
        .find(|i| i.rel_path.ends_with("personal.db"))
        .unwrap();
    let ItemTarget::Db(target) = &personal.target else {
        panic!("personal.db is a database")
    };
    let old_key = io::db_key_for(target, &fx.dek, USER, 1).unwrap();

    let reply = writer_gate::acquire(USER, Kind::Reply).unwrap();
    let stop = reply.stop_token().clone();
    let choice = match ends {
        WriterEnds::Finishes => Choice::FinishFirst,
        WriterEnds::IsStopped => Choice::StopAndSave,
    };
    let writer = async {
        // The drain has begun: no new reply may start, this one is still ours.
        while writer_gate::phase(USER) != writer_gate::Phase::Draining {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(writer_gate::acquire(USER, Kind::Reply).is_err());
        if matches!(ends, WriterEnds::IsStopped) {
            stop.cancelled().await;
        }
        // Written under the OLD key, before the rotation may start.
        mark_db(&personal.abs_path, &old_key, "writer:what the reply saved").await;
        drop(reply);
    };
    let (guard, ()) = tokio::join!(
        async {
            writer_gate::drain_for_rotation(USER, choice, DrainPolicy::default())
                .await
                .unwrap()
        },
        writer
    );

    // The rotation starts only now, with the gate closed and empty, and dies.
    let mut env = fx.env();
    env.require_closed_gate = true;
    env.faults = vec![fault];
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(
        matches!(err, RotationError::SimulatedCrash),
        "{fault:?}/{ends:?}: {err:?}"
    );
    drop(env);
    drop(guard);

    // The next unlock resumes forward and completes.
    let resumed = fx.resume_with_new_password().await.unwrap();
    let ResumeOutcome::Completed(outcome) = resumed else {
        panic!("{fault:?}/{ends:?}: expected the rotation to complete");
    };
    assert_completed(&fx, &outcome.new_dek).await;
    let new_key = io::db_key_for(target, &outcome.new_dek, USER, 2).unwrap();
    let markers = read_markers(&personal.abs_path, &new_key).await;
    assert!(
        markers.iter().any(|m| m == "writer:what the reply saved"),
        "{fault:?}/{ends:?}: the writer's data is lost: {markers:?}"
    );
    assert!(writer_gate::is_idle(USER));
}

#[tokio::test]
async fn the_kill_matrix_with_a_concurrent_writer() {
    // One fixture per case; run in sequence (each holds the process lock).
    for ends in [WriterEnds::Finishes, WriterEnds::IsStopped] {
        for fault in [
            FaultPoint::AfterStage,
            FaultPoint::AfterItemRekeyBeforeRow(0),
            FaultPoint::AfterItemCommitted(0),
            FaultPoint::AfterAllItems,
            FaultPoint::AfterPromote,
        ] {
            // (A crash after the rotation finished leaves a normal account:
            // crash_after_the_rotation_finished_leaves_a_normal_account.)
            crash_with_a_concurrent_writer_then_resume(fault, ends).await;
        }
    }
}

#[tokio::test]
async fn crash_after_staging_resumes_forward() {
    crash_then_resume_forward(FaultPoint::AfterStage).await;
}

#[tokio::test]
async fn crash_after_a_rekey_before_its_row_resumes_forward() {
    // First item, a middle one, and the last (a blob).
    crash_then_resume_forward(FaultPoint::AfterItemRekeyBeforeRow(0)).await;
    crash_then_resume_forward(FaultPoint::AfterItemRekeyBeforeRow(5)).await;
    crash_then_resume_forward(FaultPoint::AfterItemRekeyBeforeRow(10)).await;
}

#[tokio::test]
async fn crash_between_items_resumes_forward() {
    crash_then_resume_forward(FaultPoint::AfterItemCommitted(0)).await;
    crash_then_resume_forward(FaultPoint::AfterItemCommitted(7)).await;
}

#[tokio::test]
async fn crash_with_every_item_done_but_not_promoted_resumes_forward() {
    crash_then_resume_forward(FaultPoint::AfterAllItems).await;
}

#[tokio::test]
async fn crash_after_promotion_step_one_is_verified_by_the_next_unlock() {
    let fx = Fixture::new().await;
    let err = fx
        .rotate_with_fault(FaultPoint::AfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::SimulatedCrash));
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, RotationState::Verifying);
    // Promotion already made the new wraps live: the old password is dead.
    assert!(fx.unlock_live(PASSWORD).await.is_err());
    assert!(fx.unlock_live(NEW_PASSWORD).await.is_ok());

    let ResumeOutcome::Completed(outcome) = fx.resume_with_new_password().await.unwrap() else {
        panic!("expected completion");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn crash_after_the_rotation_finished_leaves_a_normal_account() {
    let fx = Fixture::new().await;
    let err = fx
        .rotate_with_fault(
            FaultPoint::AfterFinishBeforeInstall,
            Fixture::change_password(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::SimulatedCrash));
    // Nothing pending: the committed state stands and the new password logs in.
    assert_eq!(fx.wrapper().await.pending_rotation, None);
    let dek = fx.unlock_live(NEW_PASSWORD).await.unwrap();
    assert_completed(&fx, &dek).await;
}

#[tokio::test]
async fn a_stale_tmp_planted_mid_blob_is_removed_on_resume() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(
        FaultPoint::AfterItemCommitted(8),
        Fixture::change_password(),
    )
    .await
    .unwrap_err();
    // The crash left junk beside a blob that is still to be rewritten.
    let blob = ingest_blob::ingest_blob_path(USER, PERSONA, "out-2", 1);
    std::fs::write(ingest_blob::stale_tmp_path(&blob), b"partial write").unwrap();

    let ResumeOutcome::Completed(outcome) = fx.resume_with_new_password().await.unwrap() else {
        panic!("expected completion");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn the_recovery_phrase_can_resume_too() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(
        FaultPoint::AfterItemCommitted(3),
        Fixture::change_password(),
    )
    .await
    .unwrap_err();
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();

    // Wrong words are refused, the right ones resume.
    let wrong = data_key::generate_recovery_entropy().unwrap();
    assert!(matches!(
        new_dek_from_staged_phrase(&pending, USER, &wrong),
        Err(RotationError::WrongPhrase)
    ));
    let new_dek = new_dek_from_staged_phrase(&pending, USER, &fx.recovery_entropy).unwrap();
    let resumed = resume_rotation(
        &fx.env(),
        &pending,
        new_dek,
        Credential::Phrase(&fx.recovery_entropy),
    )
    .await
    .unwrap();
    let ResumeOutcome::Completed(outcome) = resumed else {
        panic!("expected completion");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

// ---------------------------------------------------------------------------
// Failure with the process alive: automatic rollback
// ---------------------------------------------------------------------------

async fn assert_rolled_back(fx: &Fixture) {
    let state = fx.wrapper().await;
    assert_eq!(state.pending_rotation, None);
    assert_eq!(state.wrapper.key_epoch, 1, "the live wraps never changed");
    assert!(user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .is_none());
    let live = fx.unlock_live(PASSWORD).await.unwrap();
    assert_eq!(*live, *fx.dek, "the old password still works");
    fx.assert_all_under(&fx.dek, 1).await;
    fx.assert_no_leftovers().await;
    assert!(!epoch_guard::is_rotating());
}

#[tokio::test]
async fn a_failure_mid_rotation_rolls_everything_back() {
    let fx = Fixture::new().await;
    let err = fx
        .rotate_with_fault(
            FaultPoint::FailAfterItemCommitted(6),
            Fixture::change_password(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::RolledBack { .. }), "{err:?}");
    assert_rolled_back(&fx).await;

    let row = sqlx::query_as::<_, (String,)>(
        "SELECT state FROM user_key_rotations ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "rolled_back");
    // The account can rotate again afterwards.
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn a_promotion_error_rolls_back_because_nothing_was_promoted() {
    let fx = Fixture::new().await;
    // Promotion step 1 updates user_salts; make that fail for real, after
    // every file has been re-keyed.
    sqlx::query(
        "CREATE TRIGGER block_promote BEFORE UPDATE ON user_salts
         BEGIN SELECT RAISE(ABORT, 'blocked by the test'); END",
    )
    .execute(&fx.pool)
    .await
    .unwrap();
    let err = fx.rotate(Fixture::change_password()).await.unwrap_err();
    assert!(matches!(err, RotationError::RolledBack { .. }), "{err:?}");
    assert_rolled_back(&fx).await;

    // Unblocked, the same account rotates cleanly.
    sqlx::query("DROP TRIGGER block_promote")
        .execute(&fx.pool)
        .await
        .unwrap();
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn a_failure_after_promotion_is_not_rolled_back_and_resumes() {
    let fx = Fixture::new().await;
    let err = fx
        .rotate_with_fault(FaultPoint::FailAfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::NeedsResume { .. }), "{err:?}");
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, RotationState::Verifying);
    assert!(!epoch_guard::is_rotating());

    let ResumeOutcome::Completed(outcome) = fx.resume_with_new_password().await.unwrap() else {
        panic!("expected completion");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn a_crash_during_rollback_is_finished_in_the_rollback_direction() {
    let fx = Fixture::new().await;
    // A real failure after item 6 starts the automatic rollback, and the
    // process dies while undoing the third file of it.
    let mut env = fx.env();
    env.faults = vec![
        FaultPoint::FailAfterItemCommitted(6),
        FaultPoint::RollbackAfterItem(8),
    ];
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");
    assert!(!epoch_guard::is_rotating());

    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.direction, RotationDirection::Rollback);

    // The next unlock continues in the recorded direction.
    let resumed = fx.resume_with_new_password().await.unwrap();
    assert!(matches!(resumed, ResumeOutcome::RolledBack));
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn cancelling_an_interrupted_rotation_rolls_it_back() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(
        FaultPoint::AfterItemCommitted(4),
        Fixture::change_password(),
    )
    .await
    .unwrap_err();
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    let pk = kdf::derive_password_key(
        NEW_PASSWORD,
        pending.salt.as_ref().unwrap(),
        pending.kdf_memory_kib.unwrap(),
        pending.kdf_iterations.unwrap(),
        pending.kdf_parallelism.unwrap(),
    )
    .unwrap();
    let new_dek = new_dek_from_staged_password(&pending, USER, &pk).unwrap();
    let out = cancel_rotation(&fx.env(), &pending, new_dek).await.unwrap();
    assert!(matches!(out, ResumeOutcome::RolledBack));
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn a_promoted_rotation_cannot_be_cancelled() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    let pk = kdf::derive_password_key(
        NEW_PASSWORD,
        pending.salt.as_ref().unwrap(),
        pending.kdf_memory_kib.unwrap(),
        pending.kdf_iterations.unwrap(),
        pending.kdf_parallelism.unwrap(),
    )
    .unwrap();
    let new_dek = new_dek_from_staged_password(&pending, USER, &pk).unwrap();
    assert!(matches!(
        cancel_rotation(&fx.env(), &pending, new_dek).await,
        Err(RotationError::AlreadyPromoted)
    ));
}

#[tokio::test]
async fn unlock_staged_reaches_the_new_key_with_the_new_password_or_the_phrase() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();

    // Wrong password, wrong phrase and the OLD password are all just wrong.
    for wrong in [&b"not it"[..], PASSWORD] {
        assert!(matches!(
            unlock_staged(&pending, USER, StagedCredential::Password(wrong)).await,
            Err(StagedUnlockError::WrongCredential)
        ));
    }
    let stranger = data_key::generate_recovery_entropy().unwrap();
    assert!(matches!(
        unlock_staged(&pending, USER, StagedCredential::Phrase(&stranger)).await,
        Err(StagedUnlockError::WrongCredential)
    ));

    // Staged cost parameters gone: a password cannot even be tried.
    let mut no_kdf = pending.clone();
    no_kdf.kdf_memory_kib = None;
    assert!(matches!(
        unlock_staged(&no_kdf, USER, StagedCredential::Password(NEW_PASSWORD)).await,
        Err(StagedUnlockError::Unusable)
    ));
    // Staged wrap gone: not a wrong credential either.
    let mut no_wrap = pending.clone();
    no_wrap.password_wrap = None;
    assert!(matches!(
        unlock_staged(&no_wrap, USER, StagedCredential::Password(NEW_PASSWORD)).await,
        Err(StagedUnlockError::Failed(RotationError::CannotResume(_)))
    ));

    // Both right credentials reach the same new key, and each supplies the
    // read-back credential its own resume needs.
    let by_password = unlock_staged(&pending, USER, StagedCredential::Password(NEW_PASSWORD))
        .await
        .unwrap();
    let by_phrase = unlock_staged(
        &pending,
        USER,
        StagedCredential::Phrase(&fx.recovery_entropy),
    )
    .await
    .unwrap();
    assert_eq!(*by_password.new_dek, *by_phrase.new_dek);
    assert!(matches!(by_password.credential(), Credential::Password(_)));
    assert!(matches!(by_phrase.credential(), Credential::Phrase(_)));

    let resumed = resume_rotation(
        &fx.env(),
        &pending,
        by_phrase.new_dek.clone(),
        by_phrase.credential(),
    )
    .await
    .unwrap();
    let ResumeOutcome::Completed(outcome) = resumed else {
        panic!("expected the rotation to complete");
    };
    assert_completed(&fx, &outcome.new_dek).await;
}

#[tokio::test]
async fn the_wrong_new_key_cannot_drive_a_resume() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .unwrap();
    let stranger = data_key::generate_dek().unwrap();
    let pk = [7u8; 32];
    let err = resume_rotation(&fx.env(), &pending, stranger, Credential::Password(&pk))
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::CannotResume(_)), "{err:?}");
    assert!(!epoch_guard::is_rotating());
}

// ---------------------------------------------------------------------------
// Resume and cancel report from the start (items.id=790)
// ---------------------------------------------------------------------------

/// The pending rotation, the new key the NEW password reaches, and the
/// read-back key, the way an unlock with the new password would produce them.
async fn pending_with_new_key(fx: &Fixture) -> (PendingRotation, SecretBytes, [u8; 32]) {
    let pending = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .expect("a rotation must be pending");
    let pk = kdf::derive_password_key(
        NEW_PASSWORD,
        pending.salt.as_ref().unwrap(),
        pending.kdf_memory_kib.unwrap(),
        pending.kdf_iterations.unwrap(),
        pending.kdf_parallelism.unwrap(),
    )
    .unwrap();
    let new_dek = new_dek_from_staged_password(&pending, USER, &pk).unwrap();
    (pending, new_dek, pk)
}

fn recorded_phases(recorder: &Recorder) -> Vec<Phase> {
    recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(e, _)| e.phase)
        .collect()
}

/// Every event carries the pending rotation's id and is emitted with the flag
/// held (so a status read agrees a rotation is running).
fn assert_events_belong_to(recorder: &Recorder, pending: &PendingRotation) {
    for (e, rotating) in recorder.events.lock().unwrap().iter() {
        assert_eq!(e.rotation_id, pending.rotation_id);
        assert!(*rotating, "every event is emitted with the flag held");
    }
}

#[tokio::test]
async fn resume_reports_before_it_loads_the_run_and_without_counts() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let resumed = resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();
    assert!(matches!(resumed, ResumeOutcome::Completed(_)));

    let events = recorder.events.lock().unwrap();
    // First: no inventory yet, so no counts. Second: the items are loaded, so
    // the report now counts bytes against the total.
    assert_eq!(events[0].0.phase, Phase::Resuming);
    assert_eq!((events[0].0.current, events[0].0.total), (None, None));
    assert_eq!(events[1].0.phase, Phase::Resuming);
    assert_eq!(events[1].0.current, Some(0));
    assert!(events[1].0.total.unwrap() > 0);
    assert_eq!(events.last().unwrap().0.phase, Phase::Complete);
    drop(events);
    assert_events_belong_to(&recorder, &pending);
}

#[tokio::test]
async fn resume_refused_by_load_run_closes_the_report_and_changes_nothing() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, _, pk) = pending_with_new_key(&fx).await;
    let stranger = data_key::generate_dek().unwrap();

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let err = resume_rotation(&env, &pending, stranger, Credential::Password(&pk))
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::CannotResume(_)), "{err:?}");

    assert_eq!(
        recorded_phases(&recorder),
        vec![Phase::Resuming, Phase::NotStarted]
    );
    for (e, _) in recorder.events.lock().unwrap().iter() {
        assert_eq!((e.current, e.total), (None, None));
    }
    assert_events_belong_to(&recorder, &pending);
    assert!(!epoch_guard::is_rotating());
    assert!(
        user_store::get_pending_rotation(&fx.pool, USER)
            .await
            .unwrap()
            .is_some(),
        "the refusal left the rotation pending and resumable"
    );
}

#[tokio::test]
async fn cancel_reports_rolling_back_before_it_loads_the_run() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(
        FaultPoint::AfterItemCommitted(4),
        Fixture::change_password(),
    )
    .await
    .unwrap_err();
    let (pending, new_dek, _) = pending_with_new_key(&fx).await;

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let out = cancel_rotation(&env, &pending, new_dek).await.unwrap();
    assert!(matches!(out, ResumeOutcome::RolledBack));

    let phases = recorded_phases(&recorder);
    assert_eq!(phases.first(), Some(&Phase::RollingBack));
    assert_eq!(phases.last(), Some(&Phase::RolledBack));
    assert!(phases[..phases.len() - 1]
        .iter()
        .all(|p| *p == Phase::RollingBack));
    {
        let events = recorder.events.lock().unwrap();
        assert_eq!((events[0].0.current, events[0].0.total), (None, None));
    }
    assert!(phases.len() > 2, "the items are reported after the first");
    assert_events_belong_to(&recorder, &pending);
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn cancel_refused_by_load_run_closes_the_report_and_changes_nothing() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, _, _) = pending_with_new_key(&fx).await;
    let stranger = data_key::generate_dek().unwrap();

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let err = cancel_rotation(&env, &pending, stranger).await.unwrap_err();
    assert!(matches!(err, RotationError::CannotResume(_)), "{err:?}");

    assert_eq!(
        recorded_phases(&recorder),
        vec![Phase::RollingBack, Phase::NotStarted]
    );
    assert_events_belong_to(&recorder, &pending);
    assert!(!epoch_guard::is_rotating());
    assert!(user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn resume_and_cancel_refused_before_the_first_report_emit_nothing() {
    use crate::auth::writer_gate::{self, Kind};

    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;

    // A live writer lease: refused with the count, after the flag, before any
    // report (instant, so nothing needs closing).
    {
        let _lease = writer_gate::acquire(USER, Kind::Command).unwrap();
        let err = resume_rotation(&env, &pending, new_dek.clone(), Credential::Password(&pk))
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::ActiveRuns(1)), "{err:?}");
        let err = cancel_rotation(&env, &pending, new_dek.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::ActiveRuns(1)), "{err:?}");
    }

    // Another rotation holds the flag: this one must not report at all, or its
    // NotStarted would close the OTHER rotation's overlay.
    {
        let _other = epoch_guard::begin_rotation("someone-else").unwrap();
        let err = resume_rotation(&env, &pending, new_dek.clone(), Credential::Password(&pk))
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::RotationInProgress), "{err:?}");
        let err = cancel_rotation(&env, &pending, new_dek.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::RotationInProgress), "{err:?}");
    }
    assert!(recorded_phases(&recorder).is_empty());
}

#[tokio::test]
async fn cancelling_a_promoted_rotation_emits_nothing() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, _) = pending_with_new_key(&fx).await;

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    assert!(matches!(
        cancel_rotation(&env, &pending, new_dek).await,
        Err(RotationError::AlreadyPromoted)
    ));
    assert!(recorded_phases(&recorder).is_empty());
}

#[tokio::test]
async fn a_promoted_resume_that_fails_its_read_back_closes_silently_and_stays_resumable() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, _) = pending_with_new_key(&fx).await;
    assert_eq!(pending.state, RotationState::Verifying);

    // A credential that does not open the live wrapper: the read-back fails.
    let wrong_pk = [7u8; 32];
    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    resume_rotation(&env, &pending, new_dek, Credential::Password(&wrong_pk))
        .await
        .unwrap_err();

    let phases = recorded_phases(&recorder);
    assert_eq!(
        phases,
        vec![
            Phase::Resuming,
            Phase::Resuming,
            Phase::Finishing,
            Phase::NotStarted
        ]
    );
    assert!(
        !phases.contains(&Phase::Failed),
        "no outcome is announced (decisions.id=904)"
    );
    assert_events_belong_to(&recorder, &pending);
    assert!(!epoch_guard::is_rotating());
    let after = user_store::get_pending_rotation(&fx.pool, USER)
        .await
        .unwrap()
        .expect("still pending");
    assert_eq!(
        after.state,
        RotationState::Verifying,
        "not marked failed: cancel must keep refusing a promoted rotation"
    );
}

#[tokio::test]
async fn a_crash_after_the_first_resume_report_closes_nothing() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterStage, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    env.faults = vec![FaultPoint::AfterItemCommitted(2)];
    let err = resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");

    let phases = recorded_phases(&recorder);
    assert_eq!(phases.first(), Some(&Phase::Resuming));
    assert!(
        !phases.contains(&Phase::NotStarted) && !phases.contains(&Phase::Failed),
        "a process death reports nothing: {phases:?}"
    );
}

/// The counted `Resuming` report (the second one) of a resume, with the events
/// that followed it.
fn counted_resuming(recorder: &Recorder) -> (ProgressEvent, Vec<ProgressEvent>) {
    let events: Vec<ProgressEvent> = recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(e, _)| e.clone())
        .collect();
    assert_eq!(events[0].phase, Phase::Resuming);
    assert_eq!(events[0].current, None, "the first report has no counts");
    assert_eq!(events[1].phase, Phase::Resuming);
    (events[1].clone(), events[2..].to_vec())
}

#[tokio::test]
async fn a_partly_done_resume_reports_the_committed_bytes_not_zero() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(
        FaultPoint::AfterItemCommitted(4),
        Fixture::change_password(),
    )
    .await
    .unwrap_err();
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    let items = user_store::list_rotation_items(&fx.pool, &pending.rotation_id)
        .await
        .unwrap();
    let committed: u64 = items
        .iter()
        .filter(|i| i.state == RotationState::Committed)
        .map(|i| i.bytes)
        .sum();
    let total: u64 = items.iter().map(|i| i.bytes).sum();
    assert!(committed > 0 && committed < total, "setup: a partial run");

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();

    let (resuming, after) = counted_resuming(&recorder);
    assert_eq!(resuming.current, Some(committed));
    assert_eq!(resuming.total, Some(total));
    // Continuous with the first item the resume re-keys: no step back or jump.
    let first_rekeying = after
        .iter()
        .find(|e| e.phase == Phase::Rekeying)
        .expect("the resume re-keys the remaining items");
    assert_eq!(first_rekeying.current, Some(committed));
    assert_eq!(
        recorder.events.lock().unwrap().last().unwrap().0.phase,
        Phase::Complete
    );
}

#[tokio::test]
async fn a_promoted_resume_reports_everything_done() {
    let fx = Fixture::new().await;
    fx.rotate_with_fault(FaultPoint::AfterPromote, Fixture::change_password())
        .await
        .unwrap_err();
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    assert_eq!(pending.state, RotationState::Verifying);

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();

    let (resuming, after) = counted_resuming(&recorder);
    let total = resuming.total.expect("counted");
    assert!(total > 0);
    assert_eq!(resuming.current, Some(total));
    let finishing = after.iter().find(|e| e.phase == Phase::Finishing).unwrap();
    assert_eq!(finishing.current, resuming.current);
}

/// An automatic rollback that dies after undoing item 8 (and every later one):
/// the pending rotation is left in the rollback direction, with those items in
/// state `RolledBack`.
async fn crash_a_rollback_part_way(fx: &Fixture) {
    let mut env = fx.env();
    env.faults = vec![
        FaultPoint::FailAfterItemCommitted(6),
        FaultPoint::RollbackAfterItem(8),
    ];
    run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
}

/// The `RollingBack` events that followed the counted `Resuming` report, as
/// their byte counts.
fn rolling_back_counts(after: &[ProgressEvent]) -> Vec<u64> {
    after
        .iter()
        .filter(|e| e.phase == Phase::RollingBack)
        .map(|e| e.current.expect("counted"))
        .collect()
}

#[tokio::test]
async fn a_resumed_rollback_starts_at_the_rolled_back_bytes_and_reaches_the_total() {
    let fx = Fixture::new().await;
    crash_a_rollback_part_way(&fx).await;
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    assert_eq!(pending.direction, RotationDirection::Rollback);
    let items = user_store::list_rotation_items(&fx.pool, &pending.rotation_id)
        .await
        .unwrap();
    let rolled_back: u64 = items
        .iter()
        .filter(|i| i.state == RotationState::RolledBack)
        .map(|i| i.bytes)
        .sum();
    let total: u64 = items.iter().map(|i| i.bytes).sum();
    let left = items
        .iter()
        .filter(|i| i.state != RotationState::RolledBack)
        .count();
    assert!(
        rolled_back > 0 && rolled_back < total,
        "setup: a partial rollback"
    );

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let out = resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::RolledBack));

    let (resuming, after) = counted_resuming(&recorder);
    assert_eq!(resuming.current, Some(rolled_back));
    assert_eq!(resuming.total, Some(total));
    let counts = rolling_back_counts(&after);
    assert_eq!(
        counts.len(),
        left,
        "only the items not yet undone are walked"
    );
    assert_eq!(
        counts.first(),
        resuming.current.as_ref(),
        "continuous with the report before it"
    );
    assert!(counts.windows(2).all(|w| w[0] <= w[1]), "{counts:?}");
    // The walk goes newest first, so the last item it reports is the lowest
    // one not yet undone; undoing it fills the bar.
    let last_bytes = items
        .iter()
        .find(|i| i.state != RotationState::RolledBack)
        .unwrap()
        .bytes;
    assert_eq!(counts.last().unwrap() + last_bytes, total);
    assert_eq!(
        recorder.events.lock().unwrap().last().unwrap().0.phase,
        Phase::RolledBack
    );
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn a_resumed_rollback_with_nothing_rolled_back_starts_at_zero() {
    let fx = Fixture::new().await;
    crash_a_rollback_part_way(&fx).await;
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    // The rollback direction is recorded but no item is marked undone yet. The
    // files that were undone stay on the old key, which the walk tolerates.
    let items = user_store::list_rotation_items(&fx.pool, &pending.rotation_id)
        .await
        .unwrap();
    for item in &items {
        user_store::set_rotation_item_state(
            &fx.pool,
            &pending.rotation_id,
            &item.rel_path,
            RotationState::Committed,
            None,
        )
        .await
        .unwrap();
    }
    let total: u64 = items.iter().map(|i| i.bytes).sum();

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let out = resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::RolledBack));

    let (resuming, after) = counted_resuming(&recorder);
    assert_eq!(resuming.current, Some(0));
    let counts = rolling_back_counts(&after);
    assert_eq!(counts.len(), items.len(), "every item is walked");
    assert_eq!(counts.first(), Some(&0));
    assert_eq!(counts.last().unwrap() + items[0].bytes, total);
    assert_rolled_back(&fx).await;
}

#[tokio::test]
async fn a_resumed_rollback_walks_a_failed_item_though_others_are_rolled_back() {
    let fx = Fixture::new().await;
    crash_a_rollback_part_way(&fx).await;
    let (pending, new_dek, pk) = pending_with_new_key(&fx).await;
    let items = user_store::list_rotation_items(&fx.pool, &pending.rotation_id)
        .await
        .unwrap();
    let rolled_back = items
        .iter()
        .filter(|i| i.state == RotationState::RolledBack)
        .count();
    let failed = items
        .iter()
        .rev()
        .find(|i| i.state == RotationState::RolledBack)
        .expect("setup: some items are rolled back");
    user_store::set_rotation_item_state(
        &fx.pool,
        &pending.rotation_id,
        &failed.rel_path,
        RotationState::Failed,
        Some("boom"),
    )
    .await
    .unwrap();

    let recorder = Recorder::default();
    let mut env = fx.env();
    env.progress = &recorder;
    let out = resume_rotation(&env, &pending, new_dek, Credential::Password(&pk))
        .await
        .unwrap();
    assert!(matches!(out, ResumeOutcome::RolledBack));

    let (resuming, after) = counted_resuming(&recorder);
    let counts = rolling_back_counts(&after);
    assert_eq!(
        counts.len(),
        items.len() - (rolled_back - 1),
        "the failed item is walked with the ones never undone"
    );
    let skipped: u64 = items
        .iter()
        .filter(|i| i.state == RotationState::RolledBack && i.rel_path != failed.rel_path)
        .map(|i| i.bytes)
        .sum();
    assert_eq!(resuming.current, Some(skipped));
    assert_rolled_back(&fx).await;
}

// ---------------------------------------------------------------------------
// Retained blob versions and stale files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retained_prior_versions_and_stale_tmp_files_are_handled() {
    let fx = Fixture::new().await;
    let v1 = ingest_blob::ingest_blob_path(USER, PERSONA, "out-1", 1);
    assert!(
        ingest_blob::stale_tmp_path(&v1).exists(),
        "setup plants a .tmp"
    );
    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();
    assert!(!ingest_blob::stale_tmp_path(&v1).exists(), ".tmp deleted");
    // v1 AND the retained v2 both open under the new key only.
    for version in [1, 2] {
        let path = ingest_blob::ingest_blob_path(USER, PERSONA, "out-1", version);
        assert!(ingest_blob::blob_opens_under(
            &path,
            USER,
            PERSONA,
            "out-1",
            version,
            &outcome.new_dek,
            2
        )
        .await
        .unwrap());
        assert!(
            !ingest_blob::blob_opens_under(&path, USER, PERSONA, "out-1", version, &fx.dek, 1)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn a_stale_wal_beside_a_database_does_not_survive_the_rotation() {
    let fx = Fixture::new().await;
    // A leftover -wal from an earlier life of personal.db.
    let personal = fx.user_dir.join("personas/persona-a/personal.db");
    let mut wal = personal.as_os_str().to_owned();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    std::fs::write(&wal, b"not a real wal").unwrap();

    let outcome = fx.rotate(Fixture::change_password()).await.unwrap();
    assert_completed(&fx, &outcome.new_dek).await;
    assert!(!wal.exists());
}

// ---------------------------------------------------------------------------
// Production epoch handling is exercised by the integration test
// tests/subkeys_resident_epoch.rs
// ---------------------------------------------------------------------------

#[test]
fn encode_hex_is_what_the_stores_use() {
    // Guards the assumption the fixture relies on when it keys files by hand.
    assert_eq!(encode_hex(&[0xab, 0x01]).as_str(), "ab01");
}

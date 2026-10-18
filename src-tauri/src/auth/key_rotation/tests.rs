// Engine tests (items.id=630 stage C). Every test builds a real account in a
// tempdir: shared.db with a wrapper, and real encrypted databases and blobs
// under the account directory. Crashes are simulated with FaultPoint, which
// leaves state exactly as a dead process would.

use std::sync::Mutex;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

use super::*;
use crate::auth::epoch_guard;
use crate::auth::registry::{key_hex, GroupKeyRegistry};
use crate::auth::subkeys::encode_hex;
use crate::conductor::concurrency::PathPriority;
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
async fn the_session_is_swapped_to_the_new_key_before_the_flag_clears() {
    let fx = Fixture::new().await;
    let registry = KeyRegistry::default();
    populate_registry(&registry, USER, *fx.dek, 1).await;
    let mut env = fx.env();
    env.registry = Some(&registry);
    let outcome = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();

    assert_eq!(
        registry.with_key(|k| (k.data_key, k.key_epoch)).await,
        Some((*outcome.new_dek, 2))
    );
    assert_eq!(epoch_guard::resident_epoch(USER), Some(2));
    assert!(epoch_guard::ensure_current(USER).await.is_ok());
    let (private, _) = sharing_keypair::derive_sharing_keypair(&outcome.new_dek, USER, 2);
    assert_eq!(
        registry.sharing_private_key().await,
        Some(private.to_bytes())
    );
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

    let total = events[0].0.total;
    assert!(total > 0);
    let mut last = 0;
    for (e, rotating) in events.iter() {
        assert_eq!(e.rotation_id, outcome.rotation_id);
        assert_eq!(e.total, total);
        assert!(e.current >= last && e.current <= total);
        last = e.current;
        assert!(*rotating, "every event is emitted with the flag held");
        if let Some(m) = &e.message {
            assert!(
                !m.contains(PERSONA) && !m.contains('/') && !m.contains(".db"),
                "labels carry no paths or ids: {m}"
            );
        }
    }
    assert_eq!(events.last().unwrap().0.current, total);
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
async fn a_running_task_blocks_the_rotation() {
    let fx = Fixture::new().await;
    let scheduler = ConductorScheduler::new();
    assert!(
        scheduler
            .acquire_run_slot("run-1", PathPriority::Interactive)
            .await
    );
    let mut env = fx.env();
    env.scheduler = Some(&scheduler);
    let err = run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap_err();
    assert!(matches!(err, RotationError::ActiveRuns(1)), "{err:?}");
    assert_untouched(&fx).await;

    scheduler.release_run_slot("run-1");
    run_rotation(&env, &fx.dek, 1, Fixture::change_password())
        .await
        .unwrap();
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

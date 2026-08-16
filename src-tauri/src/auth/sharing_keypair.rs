// src-tauri/src/auth/sharing_keypair.rs
//
// Account-creation asymmetric keypair (items.id=289, decisions.id=677,
// 2026-07-29). Foundation for both SYNCED persona-sharing (decisions.id=617)
// and group.db invitations (items.id=189/283/284) -- this module builds the
// keypair mechanism and a working encrypt-to-public-key primitive only, not
// either sharing protocol itself.
//
// CRYPTOGRAPHIC ARCHITECTURE (locked -- Jason + two external technical
// reviews, 2026-08-16): X25519 for key agreement (NOT Ed25519 -- no
// sender-authenticity requirement exists here, only that the recipient can
// decrypt), HKDF-SHA256 for all subkey derivation, ChaCha20-Poly1305 for the
// envelope AEAD. Bare primitives, no crypto_box/hpke wrapper -- matches this
// codebase's existing minimal-dependency style (see the argon2 comment in
// Cargo.toml). The raw X25519 scalar/DH output is NEVER used directly as key
// material anywhere in this module -- always through an HKDF step first (see
// derive_sharing_keypair and derive_envelope_aead_key below, and the
// dedicated regression test that checks for this).
//
// NO PERSISTED PRIVATE KEY, encrypted or otherwise (see shared_004.sql's own
// header for the schema-side statement of this): the private key is
// deterministically re-derived from the resident master key on every login,
// the same way the master key itself is re-derived from the password/
// mnemonic each login rather than ever being stored. A persisted encrypted
// copy would need a second key to encrypt it with -- which would just be the
// master key again, adding a ciphertext with no real defense-in-depth value.
// Re-derivation costs a single HKDF-SHA256 expand plus a scalar clamp,
// microseconds, negligible next to the ~100ms Argon2id master-key derivation
// already paid on every login.
//
// DOMAIN SEPARATION: derive_sharing_keypair's HKDF info string
// ("QuietRabbit-v1|x25519-sharing-key|{user_id}") is deliberately not just a
// static label. The version tag supports future crypto-agility. The purpose
// label is what prevents this derived subkey's namespace from colliding with
// any OTHER future subkey HKDF-derived from the same master key -- this
// codebase already has one such future consumer flagged
// (persistence/personal_store.rs: "HKDF per-field encryption... activates in
// Layer 8") -- reusing an info string across two purposes against the same
// IKM would make their derived outputs identical, a real bug. user_id is
// defense-in-depth: the master key already differs per account (unique
// Argon2 salt per user_salts row), but binding the derivation to the account
// id too means a future mismatched-argument bug is at least
// domain-separated rather than silently producing a reusable key.
//
// NONCE STRATEGY: encrypt_to_public_key uses a fresh random 12-byte nonce
// per call (getrandom, not a counter). Safe specifically because every call
// also generates a fresh ephemeral X25519 keypair, so the ChaCha20-Poly1305
// key derived below is unique per envelope -- the nonce-reuse-under-a-fixed-
// key scenario ChaCha20-Poly1305 nonces must avoid cannot happen across two
// different calls to this function. A counter was considered and rejected:
// it would need persistent, crash-safe state across process restarts for a
// function that should otherwise be a stateless pure encrypt -- a worse
// practical risk than a 96-bit random collision on a single message.
//
// ENVELOPE WIRE FORMAT (fixed, hand-rolled rather than using a wrapper
// library's own format -- see architecture note above):
//   [0]        u8      format version (0x01)
//   [1..33]    [u8;32] ephemeral X25519 public key
//   [33..45]   [u8;12] ChaCha20-Poly1305 nonce
//   [45..]     [u8]    ChaCha20-Poly1305 ciphertext, INCLUDING the 16-byte
//                      Poly1305 tag chacha20poly1305's own encrypt() appends
//
// open_shared_db() IS DUPLICATED here rather than reused from user_store.rs
// -- same reasoning user_store.rs's own module header gives for not reusing
// persona_store.rs's copy: each module's error type differs, and this is a
// ~12-line, zero-divergence-risk helper.
//
// SCOPE NOTE for items.id=284, not solved here: user_sharing_keys (below) is
// keyed by user_id (account-level, matching decisions.id=677's own "every
// account" framing), but pending_group_invitations.recipient_persona_id
// (shared_003.sql) references a Persona. Whatever maps persona -> owning
// user_id to look up the right public key is item 284's problem.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::auth::kdf;

/// X25519 public/private key length in bytes.
pub const PUBLIC_KEY_LEN: usize = 32;

const NONCE_LEN: usize = 12;
const AEAD_TAG_LEN: usize = 16;
const ENVELOPE_HEADER_LEN: usize = 1 + PUBLIC_KEY_LEN + NONCE_LEN;
const ENVELOPE_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum SharingKeypairError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Stored public key for user '{0}' is not valid hex and cannot be decoded")]
    CorruptPublicKey(String),
    #[error("Envelope is malformed or truncated")]
    MalformedEnvelope,
    #[error("Envelope decryption failed (authentication tag mismatch)")]
    DecryptionFailed,
    #[error("Envelope encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Could not generate random bytes: {0}")]
    RandomSource(String),
}

async fn open_shared_db() -> Result<SqliteConnection, SharingKeypairError> {
    let db_path = crate::persistence::migrations::get_data_root()
        .join("instance")
        .join("shared.db");
    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };
    let conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("journal_mode", journal_mode)
        .connect()
        .await?;
    Ok(conn)
}

fn hex_decode(user_id: &str, s: &str) -> Result<Vec<u8>, SharingKeypairError> {
    if !s.len().is_multiple_of(2) {
        return Err(SharingKeypairError::CorruptPublicKey(user_id.to_owned()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| SharingKeypairError::CorruptPublicKey(user_id.to_owned()))
        })
        .collect()
}

/// Derive this account's X25519 sharing keypair deterministically from its
/// resident master key. Pure function, no I/O -- called both at account
/// creation (to obtain the public key for the shared.db write) and on every
/// login (to populate KeyRegistry's resident UnlockedKey.sharing_private_key)
/// -- see module header on why the private key is never persisted instead.
pub fn derive_sharing_keypair(
    master_key: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
) -> (StaticSecret, PublicKey) {
    let hk = Hkdf::<Sha256>::new(None, master_key);
    let info = format!("QuietRabbit-v1|x25519-sharing-key|{user_id}");
    let mut okm = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
    hk.expand(info.as_bytes(), okm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    let private_key = StaticSecret::from(*okm);
    let public_key = PublicKey::from(&private_key);
    (private_key, public_key)
}

/// HKDF-SHA256(shared_secret) -> ChaCha20-Poly1305 key, bound to the exact
/// (ephemeral, recipient) key pair via the info string -- standard
/// HPKE-style key-schedule hygiene, on top of the raw DH output already
/// being a function of both keys. Never returns the raw DH output itself as
/// key material.
fn derive_envelope_aead_key(
    shared_secret: &x25519_dalek::SharedSecret,
    ephemeral_public: &[u8; PUBLIC_KEY_LEN],
    recipient_public: &[u8; PUBLIC_KEY_LEN],
) -> Zeroizing<[u8; PUBLIC_KEY_LEN]> {
    let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut info = Vec::with_capacity(41 + PUBLIC_KEY_LEN * 2);
    info.extend_from_slice(b"QuietRabbit-v1|x25519-envelope-aead-key|");
    info.extend_from_slice(ephemeral_public);
    info.extend_from_slice(recipient_public);

    let mut key_bytes = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
    hk.expand(&info, key_bytes.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    key_bytes
}

/// Encrypt `plaintext` to `recipient_public_key` (raw 32-byte X25519 public
/// key -- what a user_sharing_keys.public_key_hex row decodes to). Returns
/// the serialized envelope (see module header for the wire format). The
/// caller needs no keypair of its own -- per decisions.id=677 there is no
/// sender-authenticity requirement, only that the recipient can decrypt.
/// This is the primitive items.id=284 calls to encrypt a group's symmetric
/// key to an invitation recipient.
pub fn encrypt_to_public_key(
    recipient_public_key: &[u8; PUBLIC_KEY_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, SharingKeypairError> {
    let recipient_public_key = PublicKey::from(*recipient_public_key);

    // Fresh ephemeral keypair per call -- see module header on why this
    // makes the random nonce below safe.
    let mut ephemeral_scalar = Zeroizing::new([0u8; PUBLIC_KEY_LEN]);
    getrandom::fill(ephemeral_scalar.as_mut())
        .map_err(|e| SharingKeypairError::RandomSource(e.to_string()))?;
    let ephemeral_secret = StaticSecret::from(*ephemeral_scalar);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);

    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_public_key);
    let aead_key = derive_envelope_aead_key(
        &shared_secret,
        ephemeral_public.as_bytes(),
        recipient_public_key.as_bytes(),
    );

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|e| SharingKeypairError::RandomSource(e.to_string()))?;

    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce_bytes), plaintext)
        .map_err(|e| SharingKeypairError::EncryptionFailed(e.to_string()))?;

    let mut envelope = Vec::with_capacity(ENVELOPE_HEADER_LEN + ciphertext.len());
    envelope.push(ENVELOPE_VERSION);
    envelope.extend_from_slice(ephemeral_public.as_bytes());
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

/// Decrypt an envelope produced by encrypt_to_public_key, using this
/// account's own resident sharing private key (KeyRegistry::with_key exposes
/// it as UnlockedKey.sharing_private_key). Tampered/truncated envelopes
/// return Err, never a panic or silently-wrong plaintext -- ChaCha20-Poly1305
/// is an AEAD, so any bit flip in the ciphertext, tag, nonce, or the
/// envelope's embedded ephemeral public key fails authentication.
pub fn decrypt_own_envelope(
    private_key: &StaticSecret,
    envelope: &[u8],
) -> Result<Vec<u8>, SharingKeypairError> {
    if envelope.len() < ENVELOPE_HEADER_LEN + AEAD_TAG_LEN {
        return Err(SharingKeypairError::MalformedEnvelope);
    }
    if envelope[0] != ENVELOPE_VERSION {
        return Err(SharingKeypairError::MalformedEnvelope);
    }

    let ephemeral_public_bytes: [u8; PUBLIC_KEY_LEN] = envelope[1..1 + PUBLIC_KEY_LEN]
        .try_into()
        .expect("slice length checked above");
    let ephemeral_public = PublicKey::from(ephemeral_public_bytes);

    let nonce_bytes: [u8; NONCE_LEN] = envelope[1 + PUBLIC_KEY_LEN..ENVELOPE_HEADER_LEN]
        .try_into()
        .expect("slice length checked above");
    let ciphertext = &envelope[ENVELOPE_HEADER_LEN..];

    let recipient_public = PublicKey::from(private_key);
    let shared_secret = private_key.diffie_hellman(&ephemeral_public);
    let aead_key = derive_envelope_aead_key(
        &shared_secret,
        ephemeral_public.as_bytes(),
        recipient_public.as_bytes(),
    );

    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    cipher
        .decrypt(&Nonce::from(nonce_bytes), ciphertext)
        .map_err(|_| SharingKeypairError::DecryptionFailed)
}

/// Look up an account's stored public key (user_sharing_keys, shared.db).
/// Not secret -- shared.db is unencrypted and instance-wide (decisions.id=
/// 677: "fine to be readable instance-wide"). The write path lives directly
/// in user_store::create_user()'s SAVEPOINT (same-transaction atomicity with
/// the users/user_salts insert), not here.
pub async fn get_public_key(
    user_id: &str,
) -> Result<Option<[u8; PUBLIC_KEY_LEN]>, SharingKeypairError> {
    let mut conn = open_shared_db().await?;
    let row = sqlx::query("SELECT public_key_hex FROM user_sharing_keys WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(&mut conn)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let hex: String = row.get("public_key_hex");
    let bytes = hex_decode(user_id, &hex)?;
    let public_key: [u8; PUBLIC_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| SharingKeypairError::CorruptPublicKey(user_id.to_owned()))?;
    Ok(Some(public_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_master_key_and_user_id_derive_identical_keypair() {
        let master_key = [0x11u8; kdf::MASTER_KEY_LEN];
        let (priv1, pub1) = derive_sharing_keypair(&master_key, "user-a");
        let (priv2, pub2) = derive_sharing_keypair(&master_key, "user-a");
        assert_eq!(priv1.to_bytes(), priv2.to_bytes());
        assert_eq!(pub1.as_bytes(), pub2.as_bytes());
    }

    #[test]
    fn different_user_id_derives_a_different_keypair() {
        // Proves the info string's user_id component actually participates
        // in domain separation, not just the purpose label.
        let master_key = [0x22u8; kdf::MASTER_KEY_LEN];
        let (priv_a, _) = derive_sharing_keypair(&master_key, "user-a");
        let (priv_b, _) = derive_sharing_keypair(&master_key, "user-b");
        assert_ne!(priv_a.to_bytes(), priv_b.to_bytes());
    }

    #[test]
    fn derived_private_key_is_never_the_raw_master_key() {
        // The exact regression this locked architecture was chosen to
        // avoid: casually treating the master key as a raw X25519 scalar.
        // If this ever fails, the HKDF step has been skipped somewhere.
        let master_key = [0x33u8; kdf::MASTER_KEY_LEN];
        let (private_key, _) = derive_sharing_keypair(&master_key, "user-a");
        assert_ne!(private_key.to_bytes(), master_key);
    }

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let master_key = [0x44u8; kdf::MASTER_KEY_LEN];
        let (private_key, public_key) = derive_sharing_keypair(&master_key, "user-a");

        let plaintext = b"a group's symmetric key, 32 bytes of it right here!";
        let envelope = encrypt_to_public_key(public_key.as_bytes(), plaintext).unwrap();
        let decrypted = decrypt_own_envelope(&private_key, &envelope).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_private_key_fails_to_decrypt() {
        let (_, public_key_a) = derive_sharing_keypair(&[0x55u8; kdf::MASTER_KEY_LEN], "user-a");
        let (private_key_b, _) = derive_sharing_keypair(&[0x66u8; kdf::MASTER_KEY_LEN], "user-b");

        let envelope = encrypt_to_public_key(public_key_a.as_bytes(), b"secret").unwrap();
        let result = decrypt_own_envelope(&private_key_b, &envelope);

        assert!(matches!(result, Err(SharingKeypairError::DecryptionFailed)));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication_not_silently_wrong_plaintext() {
        let master_key = [0x77u8; kdf::MASTER_KEY_LEN];
        let (private_key, public_key) = derive_sharing_keypair(&master_key, "user-a");

        let mut envelope = encrypt_to_public_key(public_key.as_bytes(), b"secret").unwrap();
        let last = envelope.len() - 1;
        envelope[last] ^= 0xFF; // flip a bit inside the AEAD tag

        let result = decrypt_own_envelope(&private_key, &envelope);
        assert!(
            matches!(result, Err(SharingKeypairError::DecryptionFailed)),
            "a tampered envelope must fail AEAD verification, not decrypt to garbage"
        );
    }

    #[test]
    fn truncated_envelope_is_rejected_as_malformed_not_a_panic() {
        let master_key = [0x88u8; kdf::MASTER_KEY_LEN];
        let (private_key, _) = derive_sharing_keypair(&master_key, "user-a");

        let result = decrypt_own_envelope(&private_key, &[0x01, 0x02, 0x03]);
        assert!(matches!(
            result,
            Err(SharingKeypairError::MalformedEnvelope)
        ));
    }

    #[test]
    fn unknown_envelope_version_is_rejected() {
        let master_key = [0x99u8; kdf::MASTER_KEY_LEN];
        let (private_key, public_key) = derive_sharing_keypair(&master_key, "user-a");

        let mut envelope = encrypt_to_public_key(public_key.as_bytes(), b"secret").unwrap();
        envelope[0] = 0xFF;

        let result = decrypt_own_envelope(&private_key, &envelope);
        assert!(matches!(
            result,
            Err(SharingKeypairError::MalformedEnvelope)
        ));
    }
}

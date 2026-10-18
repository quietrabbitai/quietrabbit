// src-tauri/src/auth/data_key.rs
//
// Data-key (DEK) foundation (items.id=629, decisions.id=893, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Sections 4.0 and 5.1). Pure cryptography,
// no I/O: the persistence of the results lives in auth::user_store.
//
// KEY HIERARCHY
//   password -- Argon2id (auth::kdf) --> PK (password key)
//   PK  --ChaCha20-Poly1305--> password wrap  --> DEK (random, 32 bytes)
//   recovery entropy (random, 32 bytes) = the 24-word BIP39 mnemonic
//   entropy --HKDF-SHA256--> recovery X25519 keypair
//   DEK sealed to the recovery PUBLIC key = recovery seal
// The DEK is what the databases and blobs are encrypted under and what the
// KeyRegistry holds. Only the wrapper (password wrap, recovery public key,
// recovery seal, epoch, format version) is stored; the mnemonic, the
// recovery private key and the entropy are never stored (Architecture 8.5).
//
// WRAPPER FORMAT, version 1 (specified so a standalone reader can read it,
// decisions.id=868):
//   password wrap  = nonce (12 bytes) || ChaCha20-Poly1305 ciphertext+tag
//                    of the 32-byte DEK, key = PK (the raw 32-byte Argon2id
//                    output), authenticated data =
//                      b"QuietRabbit-v1|dek-wrap|"
//                      || u16-BE length of user_id || user_id (UTF-8)
//                      || key_epoch as u64-BE || format_version as u8.
//                    The tag is the password check: a wrong password (or any
//                    altered user id, epoch or version) fails here, before
//                    any database is opened.
//   recovery pubkey = X25519 public key of
//                    HKDF-SHA256(ikm = entropy, salt = none,
//                      info = b"QuietRabbit-v1|x25519-recovery-key|<user_id>")
//                    -- NOT epoch-bound, so the same 24 words stay valid
//                    across rotations.
//   recovery seal  = AAD-bound auth::sharing_keypair envelope (version 0x02 ||
//                    ephemeral pubkey || nonce || ciphertext+tag) of the
//                    32-byte DEK addressed to the recovery public key,
//                    authenticated data =
//                      b"QuietRabbit-v1|dek-seal|"
//                      || u16-BE length of user_id || user_id (UTF-8)
//                      || key_epoch as u64-BE || format_version as u8
//                      || the 32-byte recovery public key.
//                    Format version 1 (items.id=629) used a plain 0x01
//                    envelope with no authenticated data. The AAD stops a
//                    genuine seal being replayed into another user's row or
//                    another epoch (items.id=630). It does not stop forgery
//                    by someone who can write the row: an envelope to a
//                    public key can be built by anyone who knows it. The
//                    restore path therefore also proves the unsealed DEK by
//                    opening a database with it (auth::key_rotation).
//
// NONCE STRATEGY: a fresh random 12-byte nonce per wrap. The PK is derived
// from a per-account salt and a wrap is written once per (PK, epoch), so
// the random-nonce collision risk is negligible; no counter state is kept.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::auth::kdf;
use crate::auth::sharing_keypair;

/// Wrapper format version written by this build.
/// Version 2 (items.id=630) binds the recovery seal to user, epoch and
/// format version. No shipped users, so there is no version-1 read path.
pub const WRAP_FORMAT_VERSION: u8 = 2;

/// Key epoch of a freshly created account. Increases at every rotation
/// (items.id=630).
pub const INITIAL_KEY_EPOCH: u64 = 1;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

pub type SecretBytes = Zeroizing<[u8; kdf::MASTER_KEY_LEN]>;

#[derive(Debug, Error)]
pub enum DataKeyError {
    /// The password wrap's authentication tag did not verify: wrong password,
    /// or altered wrapper material. Indistinguishable by design.
    #[error("wrong password")]
    WrongPassword,
    #[error("key wrapper is malformed")]
    MalformedWrapper,
    #[error("recovery seal could not be opened with this recovery phrase")]
    RecoveryUnsealFailed,
    #[error("recovery phrase is not valid")]
    InvalidMnemonic,
    #[error("could not generate random bytes: {0}")]
    RandomSource(String),
    #[error("key wrapping failed: {0}")]
    Crypto(String),
}

/// The stored half of an account's key material. Ciphertext and a public key
/// only -- nothing in here opens any data without the password or the
/// mnemonic.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyWrapper {
    pub key_epoch: u64,
    pub format_version: u8,
    pub password_wrap: Vec<u8>,
    pub recovery_public_key: [u8; sharing_keypair::PUBLIC_KEY_LEN],
    pub recovery_seal: Vec<u8>,
}

impl std::fmt::Debug for KeyWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyWrapper")
            .field("key_epoch", &self.key_epoch)
            .field("format_version", &self.format_version)
            .finish_non_exhaustive()
    }
}

/// Everything produced at account creation. `dek` and `recovery_entropy` are
/// secret and zeroized on drop; `wrapper` is what gets stored.
pub struct NewAccountKeyMaterial {
    pub dek: SecretBytes,
    pub recovery_entropy: SecretBytes,
    pub wrapper: KeyWrapper,
}

impl std::fmt::Debug for NewAccountKeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccountKeyMaterial")
            .field("wrapper", &self.wrapper)
            .finish_non_exhaustive()
    }
}

fn random_key() -> Result<SecretBytes, DataKeyError> {
    let mut key = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    getrandom::fill(key.as_mut()).map_err(|e| DataKeyError::RandomSource(e.to_string()))?;
    Ok(key)
}

/// A fresh random data key.
pub fn generate_dek() -> Result<SecretBytes, DataKeyError> {
    random_key()
}

/// Fresh 256-bit recovery entropy (the 24-word phrase).
pub fn generate_recovery_entropy() -> Result<SecretBytes, DataKeyError> {
    random_key()
}

fn wrap_aad(user_id: &str, key_epoch: u64, format_version: u8) -> Vec<u8> {
    let uid = user_id.as_bytes();
    let mut aad = Vec::with_capacity(24 + 2 + uid.len() + 8 + 1);
    aad.extend_from_slice(b"QuietRabbit-v1|dek-wrap|");
    aad.extend_from_slice(&(uid.len() as u16).to_be_bytes());
    aad.extend_from_slice(uid);
    aad.extend_from_slice(&key_epoch.to_be_bytes());
    aad.push(format_version);
    aad
}

/// Encrypt the DEK under the password key. See the module header for the
/// byte layout.
pub fn wrap_dek(
    password_key: &[u8; kdf::MASTER_KEY_LEN],
    dek: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    key_epoch: u64,
    format_version: u8,
) -> Result<Vec<u8>, DataKeyError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| DataKeyError::RandomSource(e.to_string()))?;
    let aad = wrap_aad(user_id, key_epoch, format_version);
    let cipher = ChaCha20Poly1305::new(&Key::from(*password_key));
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: dek.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|e| DataKeyError::Crypto(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Unwrap the DEK with the password key. A tag failure is `WrongPassword`:
/// the password check.
pub fn unwrap_dek(
    password_key: &[u8; kdf::MASTER_KEY_LEN],
    password_wrap: &[u8],
    user_id: &str,
    key_epoch: u64,
    format_version: u8,
) -> Result<SecretBytes, DataKeyError> {
    if password_wrap.len() != NONCE_LEN + kdf::MASTER_KEY_LEN + TAG_LEN {
        return Err(DataKeyError::MalformedWrapper);
    }
    let (nonce, ciphertext) = password_wrap.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
    let aad = wrap_aad(user_id, key_epoch, format_version);
    let cipher = ChaCha20Poly1305::new(&Key::from(*password_key));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| DataKeyError::WrongPassword)?,
    );
    let mut dek = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    if plaintext.len() != dek.len() {
        return Err(DataKeyError::MalformedWrapper);
    }
    dek.copy_from_slice(&plaintext);
    Ok(dek)
}

/// The recovery X25519 keypair, derived from the mnemonic's entropy. Not
/// bound to any key epoch -- see the module header.
pub fn derive_recovery_keypair(
    entropy: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
) -> (StaticSecret, PublicKey) {
    let hk = Hkdf::<Sha256>::new(None, entropy);
    let info = format!("QuietRabbit-v1|x25519-recovery-key|{user_id}");
    let mut okm = Zeroizing::new([0u8; sharing_keypair::PUBLIC_KEY_LEN]);
    hk.expand(info.as_bytes(), okm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    let secret = StaticSecret::from(*okm);
    let public = PublicKey::from(&secret);
    (secret, public)
}

fn seal_aad(
    user_id: &str,
    key_epoch: u64,
    format_version: u8,
    recovery_public_key: &[u8; sharing_keypair::PUBLIC_KEY_LEN],
) -> Vec<u8> {
    let uid = user_id.as_bytes();
    let mut aad = Vec::with_capacity(24 + 2 + uid.len() + 8 + 1 + recovery_public_key.len());
    aad.extend_from_slice(b"QuietRabbit-v1|dek-seal|");
    aad.extend_from_slice(&(uid.len() as u16).to_be_bytes());
    aad.extend_from_slice(uid);
    aad.extend_from_slice(&key_epoch.to_be_bytes());
    aad.push(format_version);
    aad.extend_from_slice(recovery_public_key);
    aad
}

/// Seal the DEK to the stored recovery public key, bound to user, epoch and
/// format version (module header). Needs no secret beyond the DEK itself,
/// which is what lets a later rotation re-seal without the words.
pub fn seal_dek_to_recovery_key(
    recovery_public_key: &[u8; sharing_keypair::PUBLIC_KEY_LEN],
    dek: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    key_epoch: u64,
    format_version: u8,
) -> Result<Vec<u8>, DataKeyError> {
    let aad = seal_aad(user_id, key_epoch, format_version, recovery_public_key);
    sharing_keypair::encrypt_to_public_key_with_aad(recovery_public_key, dek, &aad)
        .map_err(|e| DataKeyError::Crypto(e.to_string()))
}

/// Open a recovery seal with the recovery private key. `user_id`,
/// `key_epoch` and `format_version` must be the ones the seal was made for.
pub fn unseal_dek_with_recovery_key(
    recovery_secret: &StaticSecret,
    recovery_seal: &[u8],
    user_id: &str,
    key_epoch: u64,
    format_version: u8,
) -> Result<SecretBytes, DataKeyError> {
    let public = PublicKey::from(recovery_secret);
    let aad = seal_aad(user_id, key_epoch, format_version, public.as_bytes());
    let plaintext = Zeroizing::new(
        sharing_keypair::decrypt_own_envelope_with_aad(recovery_secret, recovery_seal, &aad)
            .map_err(|_| DataKeyError::RecoveryUnsealFailed)?,
    );
    let mut dek = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    if plaintext.len() != dek.len() {
        return Err(DataKeyError::MalformedWrapper);
    }
    dek.copy_from_slice(&plaintext);
    Ok(dek)
}

/// 32 bytes of entropy to the 24-word BIP39 phrase.
pub fn mnemonic_from_entropy(
    entropy: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<Zeroizing<String>, DataKeyError> {
    bip39::Mnemonic::from_entropy(entropy)
        .map(|m| Zeroizing::new(m.to_string()))
        .map_err(|e| DataKeyError::Crypto(e.to_string()))
}

/// A typed 24-word phrase back to its 32 bytes of entropy.
pub fn entropy_from_mnemonic(phrase: &str) -> Result<SecretBytes, DataKeyError> {
    let mnemonic = bip39::Mnemonic::parse(phrase).map_err(|_| DataKeyError::InvalidMnemonic)?;
    let bytes = Zeroizing::new(mnemonic.to_entropy());
    let mut entropy = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    if bytes.len() != entropy.len() {
        return Err(DataKeyError::InvalidMnemonic);
    }
    entropy.copy_from_slice(&bytes);
    Ok(entropy)
}

fn rotation_aad(user_id: &str, from_epoch: u64, to_epoch: u64, rotation_id: &str) -> Vec<u8> {
    let uid = user_id.as_bytes();
    let rid = rotation_id.as_bytes();
    let mut aad = Vec::with_capacity(28 + 2 + uid.len() + 16 + 2 + rid.len());
    aad.extend_from_slice(b"QuietRabbit-v1|rotation-wrap|");
    aad.extend_from_slice(&(uid.len() as u16).to_be_bytes());
    aad.extend_from_slice(uid);
    aad.extend_from_slice(&from_epoch.to_be_bytes());
    aad.extend_from_slice(&to_epoch.to_be_bytes());
    aad.extend_from_slice(&(rid.len() as u16).to_be_bytes());
    aad.extend_from_slice(rid);
    aad
}

/// Encrypt the OLD data key under the NEW data key for the pending-rotation
/// row (items.id=630). Whoever can reach the new key (through the staged
/// password wrap or the staged recovery seal) can recover the old one to
/// finish or undo a rotation that was interrupted. Layout: nonce (12) ||
/// ChaCha20-Poly1305 ciphertext+tag, authenticated data binds user, both
/// epochs and the rotation id.
pub fn wrap_old_dek_under_new(
    new_dek: &[u8; kdf::MASTER_KEY_LEN],
    old_dek: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    from_epoch: u64,
    to_epoch: u64,
    rotation_id: &str,
) -> Result<Vec<u8>, DataKeyError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| DataKeyError::RandomSource(e.to_string()))?;
    let aad = rotation_aad(user_id, from_epoch, to_epoch, rotation_id);
    let cipher = ChaCha20Poly1305::new(&Key::from(*new_dek));
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: old_dek.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|e| DataKeyError::Crypto(e.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Recover the old data key from the pending-rotation row with the new one.
pub fn unwrap_old_dek_with_new(
    new_dek: &[u8; kdf::MASTER_KEY_LEN],
    wrapped: &[u8],
    user_id: &str,
    from_epoch: u64,
    to_epoch: u64,
    rotation_id: &str,
) -> Result<SecretBytes, DataKeyError> {
    if wrapped.len() != NONCE_LEN + kdf::MASTER_KEY_LEN + TAG_LEN {
        return Err(DataKeyError::MalformedWrapper);
    }
    let (nonce, ciphertext) = wrapped.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
    let aad = rotation_aad(user_id, from_epoch, to_epoch, rotation_id);
    let cipher = ChaCha20Poly1305::new(&Key::from(*new_dek));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| DataKeyError::MalformedWrapper)?,
    );
    let mut dek = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    if plaintext.len() != dek.len() {
        return Err(DataKeyError::MalformedWrapper);
    }
    dek.copy_from_slice(&plaintext);
    Ok(dek)
}

/// The two stored values that make a new DEK recoverable: the password wrap
/// under `password_key` and the recovery seal to `recovery_public_key`, both
/// for `key_epoch` and the current format version. Used to stage a rotation.
pub fn build_wrapper_parts(
    user_id: &str,
    dek: &[u8; kdf::MASTER_KEY_LEN],
    password_key: &[u8; kdf::MASTER_KEY_LEN],
    recovery_public_key: &[u8; sharing_keypair::PUBLIC_KEY_LEN],
    key_epoch: u64,
) -> Result<(Vec<u8>, Vec<u8>), DataKeyError> {
    let password_wrap = wrap_dek(password_key, dek, user_id, key_epoch, WRAP_FORMAT_VERSION)?;
    let recovery_seal = seal_dek_to_recovery_key(
        recovery_public_key,
        dek,
        user_id,
        key_epoch,
        WRAP_FORMAT_VERSION,
    )?;
    Ok((password_wrap, recovery_seal))
}

/// Build all key material for a new account: a random DEK, independent
/// recovery entropy, and the wrapper (password wrap under `password_key`,
/// recovery public key, recovery seal) at the initial epoch.
pub fn create_account_key_material(
    user_id: &str,
    password_key: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<NewAccountKeyMaterial, DataKeyError> {
    let dek = generate_dek()?;
    let recovery_entropy = generate_recovery_entropy()?;
    let (_, recovery_public) = derive_recovery_keypair(&recovery_entropy, user_id);

    let password_wrap = wrap_dek(
        password_key,
        &dek,
        user_id,
        INITIAL_KEY_EPOCH,
        WRAP_FORMAT_VERSION,
    )?;
    let recovery_seal = seal_dek_to_recovery_key(
        recovery_public.as_bytes(),
        &dek,
        user_id,
        INITIAL_KEY_EPOCH,
        WRAP_FORMAT_VERSION,
    )?;

    Ok(NewAccountKeyMaterial {
        dek,
        recovery_entropy,
        wrapper: KeyWrapper {
            key_epoch: INITIAL_KEY_EPOCH,
            format_version: WRAP_FORMAT_VERSION,
            password_wrap,
            recovery_public_key: *recovery_public.as_bytes(),
            recovery_seal,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PK: [u8; 32] = [0x5au8; 32];

    #[test]
    fn wrap_then_unwrap_round_trips() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let dek = unwrap_dek(
            &PK,
            &m.wrapper.password_wrap,
            "user-a",
            m.wrapper.key_epoch,
            m.wrapper.format_version,
        )
        .unwrap();
        assert_eq!(*dek, *m.dek);
    }

    #[test]
    fn wrong_password_key_fails_at_the_wrap_tag() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let result = unwrap_dek(
            &[0x5bu8; 32],
            &m.wrapper.password_wrap,
            "user-a",
            1,
            WRAP_FORMAT_VERSION,
        );
        assert!(matches!(result, Err(DataKeyError::WrongPassword)));
    }

    #[test]
    fn altered_user_epoch_or_version_fails_at_the_wrap_tag() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let w = &m.wrapper.password_wrap;
        assert!(matches!(
            unwrap_dek(&PK, w, "user-b", 1, WRAP_FORMAT_VERSION),
            Err(DataKeyError::WrongPassword)
        ));
        assert!(matches!(
            unwrap_dek(&PK, w, "user-a", 2, WRAP_FORMAT_VERSION),
            Err(DataKeyError::WrongPassword)
        ));
        assert!(matches!(
            unwrap_dek(&PK, w, "user-a", 1, WRAP_FORMAT_VERSION + 1),
            Err(DataKeyError::WrongPassword)
        ));
    }

    #[test]
    fn tampered_wrap_fails_and_short_wrap_is_malformed() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let mut w = m.wrapper.password_wrap.clone();
        let last = w.len() - 1;
        w[last] ^= 0xff;
        assert!(matches!(
            unwrap_dek(&PK, &w, "user-a", 1, WRAP_FORMAT_VERSION),
            Err(DataKeyError::WrongPassword)
        ));
        assert!(matches!(
            unwrap_dek(&PK, &w[..10], "user-a", 1, WRAP_FORMAT_VERSION),
            Err(DataKeyError::MalformedWrapper)
        ));
    }

    #[test]
    fn recovery_phrase_unseals_the_same_dek() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let phrase = mnemonic_from_entropy(&m.recovery_entropy).unwrap();
        assert_eq!(phrase.split_whitespace().count(), 24);

        let entropy = entropy_from_mnemonic(&phrase).unwrap();
        assert_eq!(*entropy, *m.recovery_entropy);
        let (secret, public) = derive_recovery_keypair(&entropy, "user-a");
        assert_eq!(public.as_bytes(), &m.wrapper.recovery_public_key);

        let dek = unseal_dek_with_recovery_key(&secret, &m.wrapper.recovery_seal, "user-a", 1, 2)
            .unwrap();
        assert_eq!(*dek, *m.dek);
    }

    #[test]
    fn recovery_entropy_is_independent_of_the_dek() {
        // Retired Option B: the mnemonic encoded the key itself.
        let m = create_account_key_material("user-a", &PK).unwrap();
        assert_ne!(*m.recovery_entropy, *m.dek);
    }

    #[test]
    fn a_different_phrase_cannot_unseal() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let other = generate_recovery_entropy().unwrap();
        let (secret, _) = derive_recovery_keypair(&other, "user-a");
        let result =
            unseal_dek_with_recovery_key(&secret, &m.wrapper.recovery_seal, "user-a", 1, 2);
        assert!(matches!(result, Err(DataKeyError::RecoveryUnsealFailed)));
    }

    #[test]
    fn recovery_keypair_is_user_scoped_and_deterministic() {
        let e = [0x07u8; 32];
        let (a1, _) = derive_recovery_keypair(&e, "user-a");
        let (a2, _) = derive_recovery_keypair(&e, "user-a");
        let (b, _) = derive_recovery_keypair(&e, "user-b");
        assert_eq!(a1.to_bytes(), a2.to_bytes());
        assert_ne!(a1.to_bytes(), b.to_bytes());
    }

    #[test]
    fn invalid_phrase_is_rejected() {
        assert!(matches!(
            entropy_from_mnemonic("not a valid recovery phrase at all"),
            Err(DataKeyError::InvalidMnemonic)
        ));
    }

    #[test]
    fn a_reseal_to_the_stored_public_key_needs_no_phrase() {
        // What lets a later rotation keep the same 24 words valid: seal a
        // NEW dek to the stored public key, then the old phrase opens it.
        let m = create_account_key_material("user-a", &PK).unwrap();
        let new_dek = generate_dek().unwrap();
        let seal =
            seal_dek_to_recovery_key(&m.wrapper.recovery_public_key, &new_dek, "user-a", 2, 2)
                .unwrap();
        let (secret, _) = derive_recovery_keypair(&m.recovery_entropy, "user-a");
        let opened = unseal_dek_with_recovery_key(&secret, &seal, "user-a", 2, 2).unwrap();
        assert_eq!(*opened, *new_dek);
    }

    #[test]
    fn the_seal_is_bound_to_user_epoch_and_format_version() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let (secret, _) = derive_recovery_keypair(&m.recovery_entropy, "user-a");
        let seal = &m.wrapper.recovery_seal;
        let ok = |u: &str, e: u64, v: u8| unseal_dek_with_recovery_key(&secret, seal, u, e, v);
        assert!(ok("user-a", INITIAL_KEY_EPOCH, WRAP_FORMAT_VERSION).is_ok());
        assert!(matches!(
            ok("user-b", INITIAL_KEY_EPOCH, WRAP_FORMAT_VERSION),
            Err(DataKeyError::RecoveryUnsealFailed)
        ));
        assert!(matches!(
            ok("user-a", INITIAL_KEY_EPOCH + 1, WRAP_FORMAT_VERSION),
            Err(DataKeyError::RecoveryUnsealFailed)
        ));
        assert!(matches!(
            ok("user-a", INITIAL_KEY_EPOCH, WRAP_FORMAT_VERSION + 1),
            Err(DataKeyError::RecoveryUnsealFailed)
        ));
    }

    #[test]
    fn a_plain_sharing_envelope_is_not_accepted_as_a_seal_and_vice_versa() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let (secret, public) = derive_recovery_keypair(&m.recovery_entropy, "user-a");
        // A version-1 envelope made the way sharing makes them.
        let plain = sharing_keypair::encrypt_to_public_key(public.as_bytes(), &[7u8; 32]).unwrap();
        assert!(matches!(
            unseal_dek_with_recovery_key(
                &secret,
                &plain,
                "user-a",
                INITIAL_KEY_EPOCH,
                WRAP_FORMAT_VERSION
            ),
            Err(DataKeyError::RecoveryUnsealFailed)
        ));
        // The sharing decrypt must never open a seal.
        assert!(sharing_keypair::decrypt_own_envelope(&secret, &m.wrapper.recovery_seal).is_err());
    }

    #[test]
    fn old_dek_wrapped_under_new_round_trips_and_is_bound() {
        let old = generate_dek().unwrap();
        let new = generate_dek().unwrap();
        let wrapped = wrap_old_dek_under_new(&new, &old, "user-a", 1, 2, "rot-1").unwrap();
        let back = unwrap_old_dek_with_new(&new, &wrapped, "user-a", 1, 2, "rot-1").unwrap();
        assert_eq!(*back, *old);
        for (u, f, t, r) in [
            ("user-b", 1, 2, "rot-1"),
            ("user-a", 2, 2, "rot-1"),
            ("user-a", 1, 3, "rot-1"),
            ("user-a", 1, 2, "rot-2"),
        ] {
            assert!(matches!(
                unwrap_old_dek_with_new(&new, &wrapped, u, f, t, r),
                Err(DataKeyError::MalformedWrapper)
            ));
        }
        let other = generate_dek().unwrap();
        assert!(unwrap_old_dek_with_new(&other, &wrapped, "user-a", 1, 2, "rot-1").is_err());
        assert!(unwrap_old_dek_with_new(&new, &wrapped[..10], "user-a", 1, 2, "rot-1").is_err());
    }

    #[test]
    fn build_wrapper_parts_opens_by_password_and_by_the_stored_phrase() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let new_dek = generate_dek().unwrap();
        let new_pk = [0x6cu8; 32];
        let (wrap, seal) = build_wrapper_parts(
            "user-a",
            &new_dek,
            &new_pk,
            &m.wrapper.recovery_public_key,
            2,
        )
        .unwrap();
        let by_password = unwrap_dek(&new_pk, &wrap, "user-a", 2, WRAP_FORMAT_VERSION).unwrap();
        assert_eq!(*by_password, *new_dek);
        // The SAME 24 words open the new seal: password change keeps them valid.
        let (secret, _) = derive_recovery_keypair(&m.recovery_entropy, "user-a");
        let by_phrase =
            unseal_dek_with_recovery_key(&secret, &seal, "user-a", 2, WRAP_FORMAT_VERSION).unwrap();
        assert_eq!(*by_phrase, *new_dek);
    }

    #[test]
    fn debug_output_carries_no_key_material() {
        let m = create_account_key_material("user-a", &PK).unwrap();
        let dbg = format!("{m:?} {:?}", m.wrapper);
        let dek_hex: String = m.dek.iter().map(|b| format!("{b:02x}")).collect();
        let ent_hex: String = m
            .recovery_entropy
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!dbg.contains(&dek_hex));
        assert!(!dbg.contains(&ent_hex));
        assert!(!dbg.to_lowercase().contains("password_wrap"));
    }
}

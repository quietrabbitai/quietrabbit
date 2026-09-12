// src-tauri/src/persistence/ingest_blob.rs
//
// items.id=383 (decisions.id=486): encrypted-at-rest storage for the real
// bytes of an ingested/uploaded document. outputs.content is TEXT and can't
// hold a real file (PDF, image, etc.) -- this module is the "real storage
// path" half of that resolution, with the stored bytes encrypted the same
// way every other piece of personal data in this app is (SQLCipher for
// structured data; this is the equivalent for opaque blobs).
//
// CRYPTOGRAPHIC ARCHITECTURE: deliberately mirrors auth/sharing_keypair.rs's
// locked pattern (Jason + two external technical reviews, 2026-08-16) rather
// than inventing a second scheme -- HKDF-SHA256 for subkey derivation,
// ChaCha20-Poly1305 for the AEAD, bare primitives (no wrapper library),
// fresh random nonce per write via getrandom. Unlike sharing_keypair.rs this
// is symmetric (one account, encrypting to itself), not an asymmetric
// envelope to a third party -- no ephemeral keypair/DH step needed, so the
// wire format is just [nonce][ciphertext], not a full envelope.
//
// KEY DERIVATION: the blob key is HKDF-derived from the account's resident
// master_key with a fixed purpose label plus persona_id, matching
// derive_sharing_keypair's domain-separation reasoning exactly -- the
// version tag supports future crypto-agility, the purpose label prevents
// collision with any other subkey ever HKDF-derived from the same
// master_key (sharing_keypair.rs's own header already documents one such
// sibling consumer), and persona_id is defense-in-depth binding so a future
// mismatched-argument bug is domain-separated rather than silently reusing
// key material across personas. The raw master_key is NEVER used directly
// as AEAD key material -- always through this HKDF step first.
//
// NO PERSISTED KEY: like every other key in this app, the blob key is never
// written to disk -- it is re-derived on demand from the resident
// master_key (KeyRegistry, same source output_store.rs's key_hex already
// comes from) every time a blob is written or read.
//
// FILE WIRE FORMAT:
//   [0..12)   [u8;12]  ChaCha20-Poly1305 nonce
//   [12..)    [u8]     ChaCha20-Poly1305 ciphertext, INCLUDING the 16-byte
//                      Poly1305 tag chacha20poly1305's own encrypt() appends
//
// VERSIONING: writing a new version does not delete or overwrite the old
// file -- persistence/output_store.rs::bump_ingested_document_version bumps
// outputs.storage_version and repoints outputs.storage_path at the new
// file's path (see ingest_blob_path's `v{version}` path segment); prior
// version files are retained on disk as history. This is the
// "edit and save future versions" support the bytes-vs-path resolution
// requires -- see items.id=383's plan.

use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::kdf;

const NONCE_LEN: usize = 12;
const AEAD_TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum IngestBlobError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Could not generate random bytes: {0}")]
    RandomSource(String),
    #[error("Blob encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Blob is malformed or truncated")]
    MalformedBlob,
    #[error("Blob decryption failed (authentication tag mismatch)")]
    DecryptionFailed,
}

/// Path to a specific version of an ingested document's encrypted blob.
/// Mirrors the per-user-per-persona directory convention every
/// migrate_*_db helper in persistence/migrations.rs already uses.
pub fn ingest_blob_path(user_id: &str, persona_id: &str, output_id: &str, version: i32) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("ingested_documents")
        .join(output_id)
        .join(format!("v{version}.enc"))
}

/// HKDF-derive this persona's ingest-blob AEAD key from the account's
/// resident master key. Pure function, no I/O. See module header for the
/// domain-separation reasoning (mirrors auth::sharing_keypair::
/// derive_sharing_keypair).
fn derive_ingest_blob_key(
    master_key: &[u8; kdf::MASTER_KEY_LEN],
    persona_id: &str,
) -> Zeroizing<[u8; KEY_LEN]> {
    let hk = Hkdf::<Sha256>::new(None, master_key);
    let info = format!("QuietRabbit-v1|ingest-blob-key|{persona_id}");
    let mut okm = Zeroizing::new([0u8; KEY_LEN]);
    hk.expand(info.as_bytes(), okm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    okm
}

/// Encrypt `plaintext` and write it to `path`, creating parent directories
/// as needed. Overwrites any existing file at `path` -- callers write each
/// version to its own `v{n}.enc` path (see ingest_blob_path) rather than
/// relying on this to preserve history.
pub async fn write_encrypted_blob(
    path: &Path,
    master_key: &[u8; kdf::MASTER_KEY_LEN],
    persona_id: &str,
    plaintext: &[u8],
) -> Result<(), IngestBlobError> {
    let aead_key = derive_ingest_blob_key(master_key, persona_id);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|e| IngestBlobError::RandomSource(e.to_string()))?;

    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce_bytes), plaintext)
        .map_err(|e| IngestBlobError::EncryptionFailed(e.to_string()))?;

    let mut file_bytes = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    file_bytes.extend_from_slice(&nonce_bytes);
    file_bytes.extend_from_slice(&ciphertext);

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, file_bytes).await?;
    Ok(())
}

/// Read and decrypt the blob at `path`. Tampered/truncated files return Err,
/// never a panic or silently-wrong plaintext -- same AEAD guarantee
/// auth::sharing_keypair::decrypt_own_envelope documents.
pub async fn read_encrypted_blob(
    path: &Path,
    master_key: &[u8; kdf::MASTER_KEY_LEN],
    persona_id: &str,
) -> Result<Vec<u8>, IngestBlobError> {
    let file_bytes = tokio::fs::read(path).await?;

    if file_bytes.len() < NONCE_LEN + AEAD_TAG_LEN {
        return Err(IngestBlobError::MalformedBlob);
    }

    let nonce_bytes: [u8; NONCE_LEN] = file_bytes[..NONCE_LEN]
        .try_into()
        .expect("slice length checked above");
    let ciphertext = &file_bytes[NONCE_LEN..];

    let aead_key = derive_ingest_blob_key(master_key, persona_id);
    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    cipher
        .decrypt(&Nonce::from(nonce_bytes), ciphertext)
        .map_err(|_| IngestBlobError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_key_is_never_the_raw_master_key() {
        // Same regression sharing_keypair.rs guards against: casually using
        // the master key directly as AEAD key material instead of through
        // the HKDF step.
        let master_key = [0x11u8; kdf::MASTER_KEY_LEN];
        let key = derive_ingest_blob_key(&master_key, "persona-a");
        assert_ne!(key.as_slice(), &master_key[..KEY_LEN]);
    }

    #[test]
    fn different_persona_id_derives_a_different_key() {
        let master_key = [0x22u8; kdf::MASTER_KEY_LEN];
        let key_a = derive_ingest_blob_key(&master_key, "persona-a");
        let key_b = derive_ingest_blob_key(&master_key, "persona-b");
        assert_ne!(*key_a, *key_b);
    }

    #[test]
    fn same_master_key_and_persona_id_derive_identical_key() {
        let master_key = [0x33u8; kdf::MASTER_KEY_LEN];
        let key1 = derive_ingest_blob_key(&master_key, "persona-a");
        let key2 = derive_ingest_blob_key(&master_key, "persona-a");
        assert_eq!(*key1, *key2);
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("nested").join("v1.enc");
        let master_key = [0x44u8; kdf::MASTER_KEY_LEN];
        let plaintext = b"a real uploaded document's bytes, right here";

        write_encrypted_blob(&path, &master_key, "persona-a", plaintext)
            .await
            .expect("write must succeed, including creating parent dirs");

        let decrypted = read_encrypted_blob(&path, &master_key, "persona-a")
            .await
            .expect("read must succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn wrong_persona_id_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        let master_key = [0x55u8; kdf::MASTER_KEY_LEN];

        write_encrypted_blob(&path, &master_key, "persona-a", b"secret")
            .await
            .unwrap();

        let result = read_encrypted_blob(&path, &master_key, "persona-b").await;
        assert!(matches!(result, Err(IngestBlobError::DecryptionFailed)));
    }

    #[tokio::test]
    async fn wrong_master_key_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");

        write_encrypted_blob(
            &path,
            &[0x66u8; kdf::MASTER_KEY_LEN],
            "persona-a",
            b"secret",
        )
        .await
        .unwrap();

        let result = read_encrypted_blob(&path, &[0x77u8; kdf::MASTER_KEY_LEN], "persona-a").await;
        assert!(matches!(result, Err(IngestBlobError::DecryptionFailed)));
    }

    #[tokio::test]
    async fn tampered_ciphertext_fails_authentication_not_silently_wrong_plaintext() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        let master_key = [0x88u8; kdf::MASTER_KEY_LEN];

        write_encrypted_blob(&path, &master_key, "persona-a", b"secret")
            .await
            .unwrap();

        let mut file_bytes = tokio::fs::read(&path).await.unwrap();
        let last = file_bytes.len() - 1;
        file_bytes[last] ^= 0xFF; // flip a bit inside the AEAD tag
        tokio::fs::write(&path, file_bytes).await.unwrap();

        let result = read_encrypted_blob(&path, &master_key, "persona-a").await;
        assert!(
            matches!(result, Err(IngestBlobError::DecryptionFailed)),
            "a tampered blob must fail AEAD verification, not decrypt to garbage"
        );
    }

    #[tokio::test]
    async fn truncated_blob_is_rejected_as_malformed_not_a_panic() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        tokio::fs::write(&path, [0x01, 0x02, 0x03]).await.unwrap();

        let master_key = [0x99u8; kdf::MASTER_KEY_LEN];
        let result = read_encrypted_blob(&path, &master_key, "persona-a").await;
        assert!(matches!(result, Err(IngestBlobError::MalformedBlob)));
    }

    #[test]
    fn ingest_blob_path_includes_user_persona_output_and_version_segments() {
        // get_data_root() reads the QR_DATA_ROOT env var (process-global
        // state) and panics if unset -- ENV_MUTEX + a real tempdir, same
        // pattern every other QR_DATA_ROOT-touching test in this codebase
        // uses, so this doesn't race a concurrently-running test that also
        // mutates the env var.
        let _lock = crate::test_support::ENV_MUTEX.blocking_lock();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        let path = ingest_blob_path("user-1", "persona-1", "output-1", 2);
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("user-1"));
        assert!(path_str.contains("persona-1"));
        assert!(path_str.contains("ingested_documents"));
        assert!(path_str.contains("output-1"));
        assert!(path_str.ends_with("v2.enc"));

        if let Some(v) = saved_root {
            std::env::set_var("QR_DATA_ROOT", v);
        } else {
            std::env::remove_var("QR_DATA_ROOT");
        }
    }
}

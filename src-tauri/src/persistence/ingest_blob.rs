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
// wire format is just [format][nonce][ciphertext], not a full envelope.
//
// KEY DERIVATION (items.id=630): the blob key is the AUTH Section 4.0
// per-purpose subkey, auth::subkeys::derive_db_key with purpose IngestBlob,
// scope persona_id, the account's user id and the key epoch -- i.e. the info
// string QuietRabbit-v1|ingest-blob|<persona_id>|<user_id>|epoch:<n>. The
// version tag supports future crypto-agility, the purpose label prevents
// collision with any other subkey derived from the same data key, and the
// user id and epoch mean a rotation (new data key, new epoch) changes every
// blob key. The raw data key is NEVER used directly as AEAD key material.
//
// NO PERSISTED KEY: like every other key in this app, the blob key is never
// written to disk -- it is re-derived on demand from the resident
// data key (KeyRegistry, same source output_store.rs's key_hex already
// comes from) every time a blob is written or read.
//
// FILE WIRE FORMAT (items.id=640):
//   [0]       u8       format byte, FORMAT_V1 (0x01)
//   [1..13)   [u8;12]  ChaCha20-Poly1305 nonce
//   [13..)    [u8]     ChaCha20-Poly1305 ciphertext, INCLUDING the 16-byte
//                      Poly1305 tag chacha20poly1305's own encrypt() appends
//
// CONTEXT BINDING (items.id=640): each ciphertext is authenticated against
// associated data naming the blob's own context, so a valid blob copied over
// another output's or another version's file fails to decrypt instead of
// being served as that document:
//   AAD = FORMAT_V1 || u64_be(len(persona_id)) || persona_id
//                   || u64_be(len(output_id))  || output_id
//                   || i32_be(version)
// Length-prefixed rather than separator-delimited, so the encoding is
// unambiguous for any id content. The format byte is inside the AAD so it
// cannot be altered without failing authentication. persona_id is bound
// twice (HKDF key and AAD); output_id and version are bound by the AAD only.
// There is deliberately no legacy read path (decisions.id=893 point 9):
// a file in any other format fails as UnsupportedFormat/DecryptionFailed.
//
// ATOMIC WRITES: see write_file_atomic. Reusable by anything that must
// rewrite a blob file in place (items.id=630's data-key rotation).
//
// VERSIONING: writing a new version does not delete or overwrite the old
// file -- persistence/output_store.rs::bump_ingested_document_version bumps
// outputs.storage_version and repoints outputs.storage_path at the new
// file's path (see ingest_blob_path's `v{version}` path segment); prior
// version files are retained on disk as history. This is the
// "edit and save future versions" support the bytes-vs-path resolution
// requires -- see items.id=383's plan. The version a blob is written and
// read with is the row's storage_version, so a path/version mismatch is
// caught at read time by the AAD.

use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

use crate::auth::kdf;
use crate::auth::subkeys;

const FORMAT_V1: u8 = 0x01;
const FORMAT_LEN: usize = 1;
const NONCE_LEN: usize = 12;
const AEAD_TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const MIN_FILE_LEN: usize = FORMAT_LEN + NONCE_LEN + AEAD_TAG_LEN;

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
    #[error("Blob has unsupported format byte {0:#04x}")]
    UnsupportedFormat(u8),
    #[error("Blob decryption failed (authentication tag mismatch)")]
    DecryptionFailed,
    /// The account's key epoch changed since this session unlocked
    /// (auth::epoch_guard).
    #[error("{0}")]
    KeyEpoch(#[from] crate::auth::epoch_guard::KeyEpochError),
    /// The blob key could not be derived (auth::subkeys).
    #[error("{0}")]
    Subkey(#[from] crate::auth::subkeys::SubkeyError),
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

/// Derive this persona's ingest-blob AEAD key from the data key for an
/// explicit key epoch. Pure function, no I/O (the rotation engine calls it
/// for both the old and the new epoch). See the module header.
pub(crate) fn derive_ingest_blob_key(
    data_key: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    persona_id: &str,
    epoch: u64,
) -> Result<Zeroizing<[u8; KEY_LEN]>, IngestBlobError> {
    Ok(subkeys::derive_db_key(
        data_key,
        subkeys::DbPurpose::IngestBlob,
        persona_id,
        user_id,
        epoch,
    )?)
}

/// Associated data binding a ciphertext to its (persona, output, version)
/// context. Length-prefixed so no (persona_id, output_id) pair can encode
/// to the same bytes as a different pair. See module header.
fn blob_aad(persona_id: &str, output_id: &str, version: i32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(FORMAT_LEN + 8 + persona_id.len() + 8 + output_id.len() + 4);
    aad.push(FORMAT_V1);
    for field in [persona_id, output_id] {
        aad.extend_from_slice(&(field.len() as u64).to_be_bytes());
        aad.extend_from_slice(field.as_bytes());
    }
    aad.extend_from_slice(&version.to_be_bytes());
    aad
}

/// `<path>.tmp`, the sibling temp file write_file_atomic stages into.
fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// fsync a directory so entries created or renamed in it survive power loss.
/// A no-op off unix, where a directory cannot be opened for syncing.
#[cfg(unix)]
async fn sync_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::File::open(dir).await?.sync_all().await
}

#[cfg(not(unix))]
async fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Stage `bytes` into `tmp`, sync it, and rename it over `path`.
async fn write_tmp_and_rename(tmp: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // File::create truncates, so a stale .tmp from an earlier crash is
    // replaced rather than appended to.
    let mut file = tokio::fs::File::create(tmp).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(tmp, path).await
}

/// Write `bytes` to `path` atomically: stage into `<path>.tmp`, `sync_all`,
/// rename over `path`, then fsync the parent directory (and the parent of
/// every directory this call had to create). Creates parent directories as
/// needed and overwrites any existing file at `path`.
///
/// Crash behaviour: `path` only ever holds the complete previous contents
/// or the complete new contents (or is absent if it never existed), never
/// a truncated mix. A crash before the rename leaves a stale `<path>.tmp`
/// that nothing reads and the next write to `path` replaces. On error the
/// `.tmp` is removed best-effort.
///
/// Durability contract by platform:
/// - unix: the new contents are fsynced before the rename and the directory
///   entry is fsynced after it, so once this returns Ok the write survives
///   power loss.
/// - non-unix: the directory fsync is a no-op, so durability after power
///   loss rests on the rename alone (the file data is still synced before
///   it). Atomicity (no truncated `path`) holds on every platform.
pub async fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), IngestBlobError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };

    // Directories create_dir_all is about to create, deepest first. Each
    // one's own entry in its parent must be fsynced too, or a crash could
    // drop the whole new directory chain along with the file inside it.
    let mut created: Vec<&Path> = Vec::new();
    let mut cur = Some(parent);
    while let Some(dir) = cur {
        if tokio::fs::try_exists(dir).await? {
            break;
        }
        created.push(dir);
        cur = dir.parent();
    }
    tokio::fs::create_dir_all(parent).await?;

    let tmp = tmp_path(path);
    if let Err(e) = write_tmp_and_rename(&tmp, path, bytes).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }

    sync_dir(parent).await?;
    for dir in created {
        if let Some(dir_parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_dir(dir_parent).await?;
        }
    }
    Ok(())
}

/// Encrypt `plaintext`, bound to (persona_id, output_id, version), and write
/// it to `path` via write_file_atomic, creating parent directories as
/// needed. Overwrites any existing file at `path` -- callers write each
/// version to its own `v{n}.enc` path (see ingest_blob_path) rather than
/// relying on this to preserve history.
pub async fn write_encrypted_blob(
    path: &Path,
    data_key: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    persona_id: &str,
    output_id: &str,
    version: i32,
    plaintext: &[u8],
) -> Result<(), IngestBlobError> {
    crate::auth::epoch_guard::ensure_current(user_id).await?;
    let aead_key = derive_ingest_blob_key(
        data_key,
        user_id,
        persona_id,
        subkeys::resident_or_initial_epoch(user_id),
    )?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|e| IngestBlobError::RandomSource(e.to_string()))?;

    let aad = blob_aad(persona_id, output_id, version);
    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|e| IngestBlobError::EncryptionFailed(e.to_string()))?;

    let mut file_bytes = Vec::with_capacity(FORMAT_LEN + NONCE_LEN + ciphertext.len());
    file_bytes.push(FORMAT_V1);
    file_bytes.extend_from_slice(&nonce_bytes);
    file_bytes.extend_from_slice(&ciphertext);

    write_file_atomic(path, &file_bytes).await
}

/// Read and decrypt the blob at `path`, which must have been written with
/// the same (persona_id, output_id, version). Tampered/truncated/moved files
/// return Err, never a panic or silently-wrong plaintext -- same AEAD
/// guarantee auth::sharing_keypair::decrypt_own_envelope documents.
pub async fn read_encrypted_blob(
    path: &Path,
    data_key: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    persona_id: &str,
    output_id: &str,
    version: i32,
) -> Result<Vec<u8>, IngestBlobError> {
    crate::auth::epoch_guard::ensure_current(user_id).await?;
    let file_bytes = tokio::fs::read(path).await?;

    if file_bytes.len() < MIN_FILE_LEN {
        return Err(IngestBlobError::MalformedBlob);
    }
    if file_bytes[0] != FORMAT_V1 {
        return Err(IngestBlobError::UnsupportedFormat(file_bytes[0]));
    }

    let nonce_bytes: [u8; NONCE_LEN] = file_bytes[FORMAT_LEN..FORMAT_LEN + NONCE_LEN]
        .try_into()
        .expect("slice length checked above");
    let ciphertext = &file_bytes[FORMAT_LEN + NONCE_LEN..];

    let aad = blob_aad(persona_id, output_id, version);
    let aead_key = derive_ingest_blob_key(
        data_key,
        user_id,
        persona_id,
        subkeys::resident_or_initial_epoch(user_id),
    )?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*aead_key));
    cipher
        .decrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| IngestBlobError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; kdf::MASTER_KEY_LEN] = [0x44u8; kdf::MASTER_KEY_LEN];

    #[test]
    fn derived_key_is_never_the_raw_master_key() {
        // Same regression sharing_keypair.rs guards against: casually using
        // the master key directly as AEAD key material instead of through
        // the HKDF step.
        let master_key = [0x11u8; kdf::MASTER_KEY_LEN];
        let key = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 1).unwrap();
        assert_ne!(key.as_slice(), &master_key[..KEY_LEN]);
    }

    #[test]
    fn different_persona_id_derives_a_different_key() {
        let master_key = [0x22u8; kdf::MASTER_KEY_LEN];
        let key_a = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 1).unwrap();
        let key_b = derive_ingest_blob_key(&master_key, "user-a", "persona-b", 1).unwrap();
        assert_ne!(*key_a, *key_b);
    }

    #[test]
    fn user_and_epoch_participate_in_the_blob_key() {
        let master_key = [0x28u8; kdf::MASTER_KEY_LEN];
        let base = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 1).unwrap();
        let other_user = derive_ingest_blob_key(&master_key, "user-b", "persona-a", 1).unwrap();
        let next_epoch = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 2).unwrap();
        assert_ne!(*base, *other_user);
        assert_ne!(*base, *next_epoch);
    }

    #[test]
    fn same_master_key_and_persona_id_derive_identical_key() {
        let master_key = [0x33u8; kdf::MASTER_KEY_LEN];
        let key1 = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 1).unwrap();
        let key2 = derive_ingest_blob_key(&master_key, "user-a", "persona-a", 1).unwrap();
        assert_eq!(*key1, *key2);
    }

    #[test]
    fn aad_differs_for_concatenation_ambiguous_ids() {
        // persona/output pairs whose plain concatenation (or NUL-joined
        // form, were an id to contain a NUL) would be identical.
        assert_ne!(blob_aad("ab", "c", 1), blob_aad("a", "bc", 1));
        assert_ne!(blob_aad("a\0b", "c", 1), blob_aad("a", "b\0c", 1));
        assert_ne!(blob_aad("", "abc", 1), blob_aad("abc", "", 1));
    }

    #[test]
    fn aad_differs_per_version() {
        assert_ne!(blob_aad("p", "o", 1), blob_aad("p", "o", 2));
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("nested").join("deeper").join("v1.enc");
        let plaintext = b"a real uploaded document's bytes, right here";

        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, plaintext)
            .await
            .expect("write must succeed, including creating parent dirs");

        let decrypted = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1)
            .await
            .expect("read must succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn file_starts_with_the_format_byte() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"x")
            .await
            .unwrap();

        let file_bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(file_bytes[0], FORMAT_V1);
        assert_eq!(file_bytes.len(), FORMAT_LEN + NONCE_LEN + 1 + AEAD_TAG_LEN);
    }

    #[tokio::test]
    async fn wrong_persona_id_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        let master_key = [0x55u8; kdf::MASTER_KEY_LEN];

        write_encrypted_blob(
            &path,
            &master_key,
            "user-a",
            "persona-a",
            "out-1",
            1,
            b"secret",
        )
        .await
        .unwrap();

        let result =
            read_encrypted_blob(&path, &master_key, "user-a", "persona-b", "out-1", 1).await;
        assert!(matches!(result, Err(IngestBlobError::DecryptionFailed)));
    }

    #[tokio::test]
    async fn wrong_master_key_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");

        write_encrypted_blob(
            &path,
            &[0x66u8; kdf::MASTER_KEY_LEN],
            "user-a",
            "persona-a",
            "out-1",
            1,
            b"secret",
        )
        .await
        .unwrap();

        let result = read_encrypted_blob(
            &path,
            &[0x77u8; kdf::MASTER_KEY_LEN],
            "user-a",
            "persona-a",
            "out-1",
            1,
        )
        .await;
        assert!(matches!(result, Err(IngestBlobError::DecryptionFailed)));
    }

    #[tokio::test]
    async fn blob_moved_to_another_output_id_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path_a = tempdir.path().join("out-a").join("v1.enc");
        let path_b = tempdir.path().join("out-b").join("v1.enc");

        write_encrypted_blob(
            &path_a,
            &KEY,
            "user-a",
            "persona-a",
            "out-a",
            1,
            b"document A",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(path_b.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::copy(&path_a, &path_b).await.unwrap();

        let swapped = read_encrypted_blob(&path_b, &KEY, "user-a", "persona-a", "out-b", 1).await;
        assert!(
            matches!(swapped, Err(IngestBlobError::DecryptionFailed)),
            "a blob copied under another output id must not decrypt as that output"
        );

        // The binding is to the ids, not the path: the same bytes still
        // open under their own output id wherever they sit.
        let own = read_encrypted_blob(&path_b, &KEY, "user-a", "persona-a", "out-a", 1)
            .await
            .unwrap();
        assert_eq!(own, b"document A");
    }

    #[tokio::test]
    async fn blob_moved_to_another_version_fails_to_decrypt() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let v1 = tempdir.path().join("v1.enc");
        let v2 = tempdir.path().join("v2.enc");

        write_encrypted_blob(&v1, &KEY, "user-a", "persona-a", "out-1", 1, b"version one")
            .await
            .unwrap();
        tokio::fs::copy(&v1, &v2).await.unwrap();

        let swapped = read_encrypted_blob(&v2, &KEY, "user-a", "persona-a", "out-1", 2).await;
        assert!(
            matches!(swapped, Err(IngestBlobError::DecryptionFailed)),
            "v1 bytes placed at the v2 path must not decrypt as version 2"
        );

        let own = read_encrypted_blob(&v1, &KEY, "user-a", "persona-a", "out-1", 1)
            .await
            .unwrap();
        assert_eq!(own, b"version one");
    }

    #[tokio::test]
    async fn tampered_ciphertext_fails_authentication_not_silently_wrong_plaintext() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        let master_key = [0x88u8; kdf::MASTER_KEY_LEN];

        write_encrypted_blob(
            &path,
            &master_key,
            "user-a",
            "persona-a",
            "out-1",
            1,
            b"secret",
        )
        .await
        .unwrap();

        let mut file_bytes = tokio::fs::read(&path).await.unwrap();
        let last = file_bytes.len() - 1;
        file_bytes[last] ^= 0xFF; // flip a bit inside the AEAD tag
        tokio::fs::write(&path, file_bytes).await.unwrap();

        let result =
            read_encrypted_blob(&path, &master_key, "user-a", "persona-a", "out-1", 1).await;
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
        let result =
            read_encrypted_blob(&path, &master_key, "user-a", "persona-a", "out-1", 1).await;
        assert!(matches!(result, Err(IngestBlobError::MalformedBlob)));
    }

    #[tokio::test]
    async fn unknown_format_byte_is_rejected_as_unsupported() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"secret")
            .await
            .unwrap();

        let mut file_bytes = tokio::fs::read(&path).await.unwrap();
        file_bytes[0] = 0x02;
        tokio::fs::write(&path, file_bytes).await.unwrap();

        let result = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1).await;
        assert!(matches!(
            result,
            Err(IngestBlobError::UnsupportedFormat(0x02))
        ));
    }

    #[tokio::test]
    async fn successful_write_leaves_no_tmp_file() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("out-1").join("v1.enc");

        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"secret")
            .await
            .unwrap();

        assert!(!tmp_path(&path).exists());
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("v1.enc")]);
    }

    #[tokio::test]
    async fn overwrite_replaces_contents() {
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");

        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"first")
            .await
            .unwrap();
        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"second")
            .await
            .unwrap();

        let decrypted = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1)
            .await
            .unwrap();
        assert_eq!(decrypted, b"second");
        assert!(!tmp_path(&path).exists());
    }

    #[tokio::test]
    async fn stale_partial_tmp_from_a_crash_is_ignored_and_then_replaced() {
        // Simulates a crash after the .tmp was partly written but before the
        // rename: the final file is still the previous good one.
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");

        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"old")
            .await
            .unwrap();
        tokio::fs::write(tmp_path(&path), [0x01, 0xAA, 0xBB])
            .await
            .unwrap();

        let still_old = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1)
            .await
            .unwrap();
        assert_eq!(still_old, b"old", "readers must never see the .tmp");

        write_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1, b"new")
            .await
            .expect("a stale .tmp must not block the next write");
        let now_new = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1)
            .await
            .unwrap();
        assert_eq!(now_new, b"new");
        assert!(!tmp_path(&path).exists(), "the stale .tmp must be gone");
    }

    #[tokio::test]
    async fn only_a_tmp_file_present_reads_as_not_found() {
        // Crash before the first rename of a brand-new blob: no final file.
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        tokio::fs::write(tmp_path(&path), b"complete-looking tmp")
            .await
            .unwrap();

        let result = read_encrypted_blob(&path, &KEY, "user-a", "persona-a", "out-1", 1).await;
        match result {
            Err(IngestBlobError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_rename_cleans_up_the_tmp_file() {
        // Failure injection: a non-empty directory at the final path makes
        // the rename fail after the .tmp was fully written and synced.
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        let path = tempdir.path().join("v1.enc");
        tokio::fs::create_dir_all(path.join("blocker"))
            .await
            .unwrap();

        let result = write_file_atomic(&path, b"bytes").await;
        assert!(matches!(result, Err(IngestBlobError::Io(_))));
        assert!(!tmp_path(&path).exists());
        assert!(path.is_dir(), "the blocking directory must be untouched");
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

// src-tauri/src/auth/subkeys.rs
//
// Per-database subkeys (items.id=630, stage A; decisions.id=893, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0 "Subkeys"). Pure derivation, no
// I/O: the DEK never keys a database directly. Every account-keyed database
// and blob is keyed by
//
//   HKDF-SHA256(ikm = DEK, salt = none,
//               info = "QuietRabbit-v1|<purpose>|<scope>|<user_id>|epoch:<n>")
//
// so a database's key can be handed to (or leak from) one store without
// exposing the DEK or any other database, and so a rotation (new DEK, new
// epoch) changes every one of them. Group databases are NOT derived: they are
// keyed by the random per-group key (auth::group_*, persistence::group_key_store).
//
// WHERE THE DERIVATION HAPPENS: inside each store opener and migrate_* helper,
// as the last step before `PRAGMA key`. The stores keep their `key_hex`
// parameter, which now means "the DEK, hex-encoded", so none of the ~1 500
// call sites that thread `key_hex` through the commands change.
//
// WHICH EPOCH: the openers all call epoch_guard::ensure_current first, which
// compares the epoch THIS process unlocked under with shared.db. The same
// registered epoch (epoch_guard::resident_epoch) is the epoch a store opened
// by this process must derive under. login() registers it BEFORE it opens or
// creates any database (see commands::auth). In a non-test build, deriving
// for a user with NO registered epoch is an error (NoResidentEpoch): guessing
// the initial epoch would silently key a rotated account's database with the
// wrong key. Only the lib's own unit tests (cfg(test)), which install a key
// without a real login, fall back to the initial epoch; integration tests
// and everything else must register an epoch. The rotation engine never
// relies on the registered epoch: it calls derive_db_key with both epochs
// explicitly.
//
// FORMAT RULE: the info string is delimiter-based, so no scope component or
// user id may contain '|' (or, for multi-part scopes, '/'), and none may be
// empty. A violation is an error, never a silently-different key.

use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

#[cfg(test)]
use crate::auth::data_key::INITIAL_KEY_EPOCH;
use crate::auth::epoch_guard;
use crate::auth::kdf;

/// Scope label for databases that belong to the account, not a persona.
pub const ACCOUNT_SCOPE: &str = "account";

/// What a derived key is for. The label is part of the HKDF info string, so
/// two purposes can never derive the same key from the same DEK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbPurpose {
    Personal,
    Outputs,
    Messages,
    PlanState,
    DomainContext,
    ViewCache,
    IntegrationKeys,
    CloudCookies,
    IngestBlob,
}

impl DbPurpose {
    /// Every purpose, for tests that must cover all of them.
    pub const ALL: [DbPurpose; 9] = [
        DbPurpose::Personal,
        DbPurpose::Outputs,
        DbPurpose::Messages,
        DbPurpose::PlanState,
        DbPurpose::DomainContext,
        DbPurpose::ViewCache,
        DbPurpose::IntegrationKeys,
        DbPurpose::CloudCookies,
        DbPurpose::IngestBlob,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DbPurpose::Personal => "db-personal",
            DbPurpose::Outputs => "db-outputs",
            DbPurpose::Messages => "db-messages",
            DbPurpose::PlanState => "db-plan-state",
            DbPurpose::DomainContext => "db-domain-context",
            DbPurpose::ViewCache => "db-view-cache",
            DbPurpose::IntegrationKeys => "db-integration-keys",
            DbPurpose::CloudCookies => "db-cloud-cookies",
            DbPurpose::IngestBlob => "ingest-blob",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubkeyError {
    /// The data key was not hex of exactly 32 bytes.
    #[error("data key is not 32 bytes of hex")]
    InvalidKeyHex,
    /// A scope or user id was empty or contained a reserved delimiter.
    #[error("key scope component is empty or contains a reserved character: {0}")]
    InvalidComponent(String),
    /// No key epoch is registered for this user in this process, so the epoch
    /// to derive under is unknown. A store was opened without a login.
    #[error("no key epoch is registered for user '{0}'")]
    NoResidentEpoch(String),
}

fn check_component(c: &str, forbid_slash: bool) -> Result<(), SubkeyError> {
    if c.is_empty() || c.contains('|') || (forbid_slash && c.contains('/')) {
        return Err(SubkeyError::InvalidComponent(c.to_owned()));
    }
    Ok(())
}

/// Join the parts of a multi-part scope (`persona/focus/topic`). Each part is
/// validated; the result is validated again as a whole by the derivation.
pub fn scope(parts: &[&str]) -> Result<String, SubkeyError> {
    for p in parts {
        check_component(p, true)?;
    }
    Ok(parts.join("/"))
}

/// Derive one database's (or blob's) key from the DEK for an explicit epoch.
/// Pure. This is the function the rotation engine calls for both the old and
/// the new epoch.
pub fn derive_db_key(
    dek: &[u8; kdf::MASTER_KEY_LEN],
    purpose: DbPurpose,
    scope: &str,
    user_id: &str,
    epoch: u64,
) -> Result<Zeroizing<[u8; kdf::MASTER_KEY_LEN]>, SubkeyError> {
    check_component(scope, false)?;
    check_component(user_id, true)?;
    let hk = Hkdf::<Sha256>::new(None, dek);
    let info = format!(
        "QuietRabbit-v1|{}|{scope}|{user_id}|epoch:{epoch}",
        purpose.label()
    );
    let mut okm = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    hk.expand(info.as_bytes(), okm.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    Ok(okm)
}

fn decode_dek_hex(s: &str) -> Result<Zeroizing<[u8; kdf::MASTER_KEY_LEN]>, SubkeyError> {
    if s.len() != kdf::MASTER_KEY_LEN * 2 || !s.is_ascii() {
        return Err(SubkeyError::InvalidKeyHex);
    }
    let mut out = Zeroizing::new([0u8; kdf::MASTER_KEY_LEN]);
    for (i, byte) in out.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| SubkeyError::InvalidKeyHex)?;
    }
    Ok(out)
}

pub(crate) fn encode_hex(bytes: &[u8]) -> Zeroizing<String> {
    let mut s = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Like derive_db_key, taking and returning hex: the form the SQLCipher
/// `PRAGMA key = "x'...'"` literal wants.
pub fn derive_db_key_hex(
    dek_hex: &str,
    purpose: DbPurpose,
    scope: &str,
    user_id: &str,
    epoch: u64,
) -> Result<Zeroizing<String>, SubkeyError> {
    let dek = decode_dek_hex(dek_hex)?;
    let key = derive_db_key(&dek, purpose, scope, user_id, epoch)?;
    Ok(encode_hex(key.as_ref()))
}

/// The epoch a store opened by this process derives under: the one this
/// process unlocked under. With none registered this is an error in a
/// non-test build, and the initial epoch only under cfg(test) (see the module
/// header).
pub fn resident_or_initial_epoch(user_id: &str) -> Result<u64, SubkeyError> {
    match epoch_guard::resident_epoch(user_id) {
        Some(epoch) => Ok(epoch),
        #[cfg(test)]
        None => Ok(INITIAL_KEY_EPOCH),
        #[cfg(not(test))]
        None => Err(SubkeyError::NoResidentEpoch(user_id.to_owned())),
    }
}

/// What every opener and migrate_* helper calls: the subkey (hex) for this
/// process's resident epoch.
pub fn db_key_hex(
    user_id: &str,
    dek_hex: &str,
    purpose: DbPurpose,
    scope: &str,
) -> Result<Zeroizing<String>, SubkeyError> {
    derive_db_key_hex(
        dek_hex,
        purpose,
        scope,
        user_id,
        resident_or_initial_epoch(user_id)?,
    )
}

/// Test helper: the subkey (hex) a store opened by this process would use for
/// `dek_hex`, for tests that open a store's file directly to inspect it.
#[cfg(test)]
pub(crate) fn test_key_hex(
    user_id: &str,
    dek_hex: &str,
    purpose: DbPurpose,
    scope: &str,
) -> String {
    db_key_hex(user_id, dek_hex, purpose, scope)
        .expect("test key must be 32 bytes of hex")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEK: [u8; 32] = [0x42u8; 32];

    fn k(purpose: DbPurpose, scope: &str, user: &str, epoch: u64) -> [u8; 32] {
        *derive_db_key(&DEK, purpose, scope, user, epoch).unwrap()
    }

    #[test]
    fn derivation_is_deterministic() {
        assert_eq!(
            k(DbPurpose::Personal, "p1", "u1", 1),
            k(DbPurpose::Personal, "p1", "u1", 1)
        );
    }

    #[test]
    fn the_subkey_is_never_the_dek() {
        for purpose in DbPurpose::ALL {
            assert_ne!(k(purpose, "p1", "u1", 1), DEK);
        }
    }

    #[test]
    fn every_input_participates_in_domain_separation() {
        let base = k(DbPurpose::Personal, "p1", "u1", 1);
        assert_ne!(base, k(DbPurpose::Outputs, "p1", "u1", 1), "purpose");
        assert_ne!(base, k(DbPurpose::Personal, "p2", "u1", 1), "scope");
        assert_ne!(base, k(DbPurpose::Personal, "p1", "u2", 1), "user");
        assert_ne!(base, k(DbPurpose::Personal, "p1", "u1", 2), "epoch");
        let other_dek = derive_db_key(&[0x43u8; 32], DbPurpose::Personal, "p1", "u1", 1).unwrap();
        assert_ne!(base, *other_dek, "dek");
    }

    #[test]
    fn all_purposes_derive_distinct_keys() {
        let mut seen = std::collections::HashSet::new();
        for purpose in DbPurpose::ALL {
            assert!(seen.insert(k(purpose, "same", "u1", 1)), "{purpose:?}");
        }
        let labels: std::collections::HashSet<_> =
            DbPurpose::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(labels.len(), DbPurpose::ALL.len());
    }

    #[test]
    fn info_string_matches_the_documented_form() {
        // Recompute by hand: the format is part of the on-disk contract.
        let hk = Hkdf::<Sha256>::new(None, &DEK);
        let mut want = [0u8; 32];
        hk.expand(
            b"QuietRabbit-v1|db-personal|persona-a|user-a|epoch:3",
            &mut want,
        )
        .unwrap();
        assert_eq!(k(DbPurpose::Personal, "persona-a", "user-a", 3), want);
    }

    #[test]
    fn delimiters_and_empty_components_are_rejected() {
        let d = |scope: &str, user: &str| derive_db_key(&DEK, DbPurpose::Personal, scope, user, 1);
        assert!(d("", "u").is_err());
        assert!(d("p", "").is_err());
        assert!(d("a|b", "u").is_err());
        assert!(d("p", "u|v").is_err());
        assert!(d("p", "u/v").is_err());
        assert!(scope(&["a", ""]).is_err());
        assert!(scope(&["a/b", "c"]).is_err());
        assert!(scope(&["a|b"]).is_err());
        assert_eq!(scope(&["p", "f", "t"]).unwrap(), "p/f/t");
        // Scope parts are separated, so ("a/b") cannot collide with ("a","b").
        assert!(d("a/b", "u").is_ok());
    }

    #[test]
    fn hex_form_round_trips_and_requires_exactly_32_bytes() {
        let dek_hex = "42".repeat(32);
        let hex = derive_db_key_hex(&dek_hex, DbPurpose::Messages, "p", "u", 1).unwrap();
        assert_eq!(hex.len(), 64);
        assert_eq!(
            hex.as_str(),
            encode_hex(&k(DbPurpose::Messages, "p", "u", 1)).as_str()
        );
        for bad in [
            "",
            "42",
            &"42".repeat(31),
            &"42".repeat(33),
            &"zz".repeat(32),
        ] {
            assert_eq!(
                derive_db_key_hex(bad, DbPurpose::Messages, "p", "u", 1).unwrap_err(),
                SubkeyError::InvalidKeyHex,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unregistered_epoch_falls_back_to_the_initial_epoch() {
        // cfg(test) only: the non-test build errors, see
        // tests/subkeys_resident_epoch.rs.
        assert_eq!(
            resident_or_initial_epoch("subkeys-never-registered"),
            Ok(INITIAL_KEY_EPOCH)
        );
    }

    /// Every source file that applies an account-scoped SQLCipher key must
    /// derive it through this module, or be on this list with its reason.
    /// Coarse (file-level) on purpose: its job is to catch a NEW opener file
    /// that hands the data key straight to SQLCipher.
    const EXEMPT: &[(&str, &str)] = &[
        (
            "providers/utils.rs",
            "defines connect_options_encrypted, opens nothing",
        ),
        (
            "persistence/group_store.rs",
            "group.db is keyed by the random group key, not a DEK subkey",
        ),
        (
            "group_sync/engine.rs",
            "group.db is keyed by the random group key, not a DEK subkey",
        ),
        (
            "conductor/executor.rs",
            "key applied only inside #[cfg(test)] code",
        ),
        (
            "commands/auth.rs",
            "key applied only inside #[cfg(test)] code",
        ),
        (
            "auth/epoch_guard.rs",
            "mentions the patterns only inside its own scan test",
        ),
        ("auth/subkeys.rs", "this scan test"),
    ];

    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_account_keyed_opener_file_derives_its_key_through_subkeys() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        let mut missing = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            // Only production code counts: a file whose key application is
            // all inside its #[cfg(test)] module is not an opener.
            let production = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            // The patterns are built with concat! so this file's own source
            // never contains them (other scan tests read this file too).
            let applies_key = production.contains(concat!("pragma(\"", "key\""))
                || production.contains(concat!("connect_options_", "encrypted("));
            if !applies_key || EXEMPT.iter().any(|(f, _)| *f == rel) {
                continue;
            }
            if !text.contains("subkeys::") {
                missing.push(rel);
            }
        }
        assert!(
            missing.is_empty(),
            "these files apply a SQLCipher key but never derive it through auth::subkeys \
             (derive the subkey, or list the file in EXEMPT with a reason): {missing:?}"
        );
    }
}

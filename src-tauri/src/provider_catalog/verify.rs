// src-tauri/src/provider_catalog/verify.rs
//
// Signature check for the catalog manifest (items.id=713). The signature is a
// detached minisign signature (`catalog-manifest.json.minisig`) made offline
// with the catalog key (Chat-PM ruling D2 = a: an offline key, not the app
// updater key, and no key of any kind is generated or stored by this code).
// Verification uses minisign-verify, zero dependencies.
//
// FAILS CLOSED. No keys compiled in means NoTrustedKey, not "skip the check".
// A malformed key in the list is skipped (and logged) rather than trusted, and
// a list in which no key verifies is BadSignature. Legacy (non-prehashed)
// minisign signatures are refused. The trusted comment is covered by minisign's
// own global signature but carries no meaning here.

use minisign_verify::{PublicKey, Signature};

/// The catalog public keys the app trusts: minisign public keys as the single
/// base64 line of a `.pub` file (or the whole two-line file). At most two are
/// expected (the current key and a pre-shipped next key, so a rotation is an
/// ordinary release).
///
/// EMPTY ON PURPOSE until Jason supplies the production public key. With an
/// empty list `check_catalog_update` returns `catalog.error.no_trusted_key`
/// before it contacts any server. RELEASE PREREQUISITE (items.id=713).
pub const TRUSTED_PUBLIC_KEYS: &[&str] = &[];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The key list is empty.
    NoTrustedKey,
    /// The signature text could not be read, or no listed key verifies it.
    BadSignature,
}

fn parse_key(text: &str) -> Option<PublicKey> {
    let t = text.trim();
    if t.contains('\n') {
        PublicKey::decode(t).ok()
    } else {
        PublicKey::from_base64(t).ok()
    }
}

/// Verify `manifest` against the detached minisign signature `signature_text`
/// using any key in `keys`.
pub fn verify_manifest(
    keys: &[&str],
    manifest: &[u8],
    signature_text: &str,
) -> Result<(), VerifyError> {
    if keys.is_empty() {
        return Err(VerifyError::NoTrustedKey);
    }
    let signature = Signature::decode(signature_text).map_err(|_| VerifyError::BadSignature)?;
    for key_text in keys {
        let Some(key) = parse_key(key_text) else {
            log::warn!("provider_catalog: a compiled-in public key could not be read");
            continue;
        };
        if key.verify(manifest, &signature, false).is_ok() {
            return Ok(());
        }
    }
    Err(VerifyError::BadSignature)
}

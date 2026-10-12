// src-tauri/src/provider_credentials/oauth/pkce.rs
//
// PKCE (RFC 7636) with S256, and the random `state` value (items.id=734).
//
// The verifier is 32 random bytes as unpadded base64url: 43 characters, the
// RFC's minimum length, 256 bits. The challenge is the unpadded base64url of
// its SHA-256. The verifier is zeroized on drop and never printed.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const CHALLENGE_METHOD: &str = "S256";

pub struct Pkce {
    verifier: Zeroizing<String>,
    challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self, String> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut_slice()).map_err(|e| e.to_string())?;
        Ok(Self::from_verifier(Zeroizing::new(
            URL_SAFE_NO_PAD.encode(bytes.as_slice()),
        )))
    }

    pub(crate) fn from_verifier(verifier: Zeroizing<String>) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// A zeroizing copy of the verifier, for paste mode's session.
    pub fn verifier_owned(&self) -> Zeroizing<String> {
        self.verifier.clone()
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pkce([redacted])")
    }
}

/// A fresh `state`: 256 random bits, unpadded base64url.
pub fn new_state() -> Result<Zeroizing<String>, String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(bytes.as_mut_slice()).map_err(|e| e.to_string())?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_slice())))
}

/// A random flow id (for telling one flow's task from the next one's).
pub fn new_flow_id() -> Result<u64, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

/// Compare two secrets without stopping at the first difference.
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_matches_the_rfc_7636_appendix_b_vector() {
        let p = Pkce::from_verifier(Zeroizing::new(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned(),
        ));
        assert_eq!(p.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn a_generated_verifier_is_long_url_safe_and_fresh() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_eq!(a.verifier().len(), 43);
        assert!(a
            .verifier()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a.verifier(), b.verifier());
        assert_eq!(a.challenge().len(), 43);
        assert_eq!(format!("{a:?}"), "Pkce([redacted])");
    }

    #[test]
    fn a_state_carries_at_least_128_bits_and_is_fresh() {
        let a = new_state().unwrap();
        let b = new_state().unwrap();
        // 43 base64url characters decode to 32 bytes.
        let raw = URL_SAFE_NO_PAD.decode(a.as_bytes()).unwrap();
        assert!(raw.len() * 8 >= 128);
        assert_ne!(*a, *b);
    }

    #[test]
    fn same_secret_is_exact() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
        assert!(!same_secret("", "a"));
        assert!(same_secret("", ""));
    }
}

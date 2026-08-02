// src-tauri/src/auth/kdf.rs
//
// Argon2id master-key derivation (items.id=205, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Section 4.1).
//
// PARAMETER OWNERSHIP (ChatGPT external review, 2026-08-01, resolved this
// session): the architecture doc stores KDF cost parameters per-account in
// user_salts (kdf_memory_kib/kdf_iterations/kdf_parallelism), explicitly so
// "a future tuning change is a data change, not a schema change" (Section
// 4.1). Given that, this module's derive_master_key() takes those
// parameters as explicit arguments and never reads a compiled constant at
// derivation time -- there is exactly one authoritative source (the
// database row), not two. The DEFAULT_* constants below exist ONLY for
// whoever writes the new-account bootstrap flow (a later step) to populate
// a fresh user_salts row with -- they are write-time defaults, not
// read-time authority, and must not be read by derive_master_key itself.
//
// API SHAPE: password and the derived key are both raw bytes (&[u8] in,
// [u8; N] out), not String/&str -- hash_password_into() takes &[u8]
// natively, so a &str parameter would only add an internal .as_bytes()
// conversion for no benefit, and staying byte-oriented avoids forcing UTF-8
// validation into this module's concern.
//
// TRANSIENT COPIES: hash_password_into() writes directly into the
// caller-supplied output buffer -- there is no second internal allocation
// on the crate's side that this module then copies out of. The 32-byte
// output here is the only copy this module creates. Zeroizing that output
// once the caller (the future KeyRegistry, a later step) is done with it is
// the caller's responsibility, not this module's -- this module creates
// bytes and returns them, owns no long-lived state, and holds no secret
// material after derive_master_key() returns.

use argon2::{Algorithm, Argon2, Params, Version};

/// Master key length in bytes (Section 4.1: "master key's raw 32 bytes
/// (256 bits of entropy)").
pub const MASTER_KEY_LEN: usize = 32;

/// Salt length in bytes -- the argon2 crate reference material's own
/// "recommended salt length for password hashing" value.
pub const SALT_LEN: usize = 16;

/// Write-time defaults for a NEW account's user_salts row (Section 4.1:
/// m=64MiB, t=3, p=4). NOT read by derive_master_key() -- see module header.
pub const DEFAULT_ARGON2_MEMORY_KIB: u32 = 65536;
pub const DEFAULT_ARGON2_ITERATIONS: u32 = 3;
pub const DEFAULT_ARGON2_PARALLELISM: u32 = 4;

/// Errors from this module. Wraps argon2::Error and getrandom::Error rather
/// than exposing either type directly to callers.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("key derivation failed: {0}")]
    Derivation(String),
    #[error("could not generate random salt: {0}")]
    RandomSource(String),
}

/// Generate a cryptographically random salt via getrandom (OS entropy
/// source), explicit direct dependency -- see Cargo.toml comment.
pub fn generate_salt() -> Result<[u8; SALT_LEN], AuthError> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|e| AuthError::RandomSource(e.to_string()))?;
    Ok(salt)
}

/// Derive a MASTER_KEY_LEN-byte master key from a password and salt using
/// Argon2id, with explicit cost parameters supplied by the caller (read
/// from that account's own user_salts row -- see module header on
/// parameter ownership).
///
/// password: raw bytes, not required to be valid UTF-8.
/// salt: any length argon2::Params/Argon2 itself accepts; SALT_LEN (16) is
///   this module's own generation length but is not enforced here, since a
///   caller reading an existing account's stored salt should not have this
///   function silently reject a salt it did not generate itself.
pub fn derive_master_key(
    password: &[u8],
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<[u8; MASTER_KEY_LEN], AuthError> {
    let params = Params::new(memory_kib, iterations, parallelism, Some(MASTER_KEY_LEN))
        .map_err(|e| AuthError::Derivation(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut output = [0u8; MASTER_KEY_LEN];
    argon2
        .hash_password_into(password, salt, &mut output)
        .map_err(|e| AuthError::Derivation(e.to_string()))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_password_and_salt_yield_identical_key() {
        let salt = generate_salt().unwrap();
        let k1 = derive_master_key(b"correct horse battery staple", &salt, 1024, 1, 1).unwrap();
        let k2 = derive_master_key(b"correct horse battery staple", &salt, 1024, 1, 1).unwrap();
        assert_eq!(k1, k2, "same password+salt+params must derive identically");
    }

    #[test]
    fn different_salt_yields_different_key() {
        let salt_a = generate_salt().unwrap();
        let salt_b = generate_salt().unwrap();
        assert_ne!(salt_a, salt_b, "two generated salts should not collide");

        let k1 = derive_master_key(b"same password", &salt_a, 1024, 1, 1).unwrap();
        let k2 = derive_master_key(b"same password", &salt_b, 1024, 1, 1).unwrap();
        assert_ne!(k1, k2, "different salts must derive different keys");
    }

    #[test]
    fn generated_salt_is_correct_length() {
        let salt = generate_salt().unwrap();
        assert_eq!(salt.len(), SALT_LEN);
    }

    #[test]
    fn generated_salts_differ_across_calls() {
        // Sanity check, not a statistical proof -- confirms getrandom is
        // actually being invoked per call, not returning a fixed buffer.
        let salts: Vec<[u8; SALT_LEN]> = (0..5).map(|_| generate_salt().unwrap()).collect();
        for i in 0..salts.len() {
            for j in (i + 1)..salts.len() {
                assert_ne!(salts[i], salts[j], "generated salts must not repeat");
            }
        }
    }

    #[test]
    fn derived_key_has_correct_length() {
        let salt = generate_salt().unwrap();
        let key = derive_master_key(b"password", &salt, 1024, 1, 1).unwrap();
        assert_eq!(key.len(), MASTER_KEY_LEN);
    }

    #[test]
    fn invalid_parallelism_returns_error_not_panic() {
        let salt = generate_salt().unwrap();
        // p=0 is outside argon2::Params' valid range.
        let result = derive_master_key(b"password", &salt, 1024, 1, 0);
        assert!(
            result.is_err(),
            "invalid Argon2 parallelism must error, not panic"
        );
    }

    #[test]
    fn invalid_memory_too_low_returns_error_not_panic() {
        let salt = generate_salt().unwrap();
        // Argon2's minimum memory cost is 8*parallelism KiB; 1 KiB with
        // parallelism=4 is well under that floor.
        let result = derive_master_key(b"password", &salt, 1, 3, 4);
        assert!(
            result.is_err(),
            "memory cost below the algorithm's floor must error, not panic"
        );
    }

    #[test]
    fn real_default_parameters_derive_successfully() {
        // Uses the actual Section 4.1 defaults (m=64MiB, t=3, p=4) -- slower
        // than the other tests' cheap params, but confirms the real
        // production configuration actually works end-to-end, not just a
        // cheap test-only parameter set.
        let salt = generate_salt().unwrap();
        let key = derive_master_key(
            b"a real-shaped password",
            &salt,
            DEFAULT_ARGON2_MEMORY_KIB,
            DEFAULT_ARGON2_ITERATIONS,
            DEFAULT_ARGON2_PARALLELISM,
        );
        assert!(
            key.is_ok(),
            "production Argon2id parameters must derive successfully"
        );
    }
}

// src-tauri/src/auth/kdf.rs
//
// Argon2id password-key derivation (items.id=205, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Section 4.1). Since items.id=629
// (decisions.id=893) the output is the PASSWORD KEY (PK), which unwraps the
// random data key (auth::data_key) -- it is no longer the key the databases
// are encrypted under.
//
// PARAMETER OWNERSHIP (ChatGPT external review, 2026-08-01, resolved this
// session): the architecture doc stores KDF cost parameters per-account in
// user_salts (kdf_memory_kib/kdf_iterations/kdf_parallelism), explicitly so
// "a future tuning change is a data change, not a schema change" (Section
// 4.1). Given that, this module's derive_password_key() takes those
// parameters as explicit arguments and never reads a compiled constant at
// derivation time -- there is exactly one authoritative source (the
// database row), not two. The DEFAULT_* constants below exist ONLY for
// whoever writes the new-account bootstrap flow (a later step) to populate
// a fresh user_salts row with -- they are write-time defaults, not
// read-time authority, and must not be read by derive_password_key itself.
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
// material after derive_password_key() returns.

// UNICODE NORMALISATION (items.id=777 stage 0, decisions.id=858): the password
// is normalised to NFKC INSIDE derive_password_key, the single choke point for
// every sign-in, change, restore, resume and cancel path, so no caller can
// forget it and the same typing always derives the same key whichever form the
// keyboard, IME or operating system produced (NFC vs NFD, full-width, ligature).
// ASCII is already NFKC, so it reaches Argon2 byte for byte as before. Input
// that is not valid UTF-8 cannot be normalised and passes through unchanged.
// This is a one-way compatibility fold (full-width "A" becomes "A"), which is
// what NFKC means: an account keyed from a raw, not-NFKC-stable password before
// this change no longer opens with that typing and is reached through the
// recovery phrase (restore does not use the old password).
use argon2::{Algorithm, Argon2, Params, Version};
use unicode_normalization::{is_nfkc, UnicodeNormalization};
use zeroize::Zeroizing;

/// A password as it is fed to the KDF: the caller's own bytes when they are
/// already normal (or not UTF-8), otherwise a zeroizing normalised copy.
pub enum NormalisedPassword<'a> {
    Borrowed(&'a [u8]),
    Owned(Zeroizing<Vec<u8>>),
}

impl NormalisedPassword<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            NormalisedPassword::Borrowed(b) => b,
            NormalisedPassword::Owned(v) => v,
        }
    }
}

/// NFKC-normalise `password` if it is valid UTF-8 and not already NFKC.
pub fn normalise_password(password: &[u8]) -> NormalisedPassword<'_> {
    let Ok(text) = std::str::from_utf8(password) else {
        return NormalisedPassword::Borrowed(password);
    };
    if is_nfkc(text) {
        return NormalisedPassword::Borrowed(password);
    }
    let mut out = Zeroizing::new(String::with_capacity(text.len()));
    out.extend(text.nfkc());
    NormalisedPassword::Owned(Zeroizing::new(out.as_bytes().to_vec()))
}

/// Key length in bytes for the PK, the DEK and group keys (all raw 32 bytes,
/// 256 bits). Name kept from before decisions.id=893 -- it is shared with
/// the group-key types.
pub const MASTER_KEY_LEN: usize = 32;

/// Salt length in bytes -- the argon2 crate reference material's own
/// "recommended salt length for password hashing" value.
pub const SALT_LEN: usize = 16;

/// Write-time defaults for a NEW account's user_salts row (Section 4.1:
/// m=64MiB, t=3, p=4). NOT read by derive_password_key() -- see module header.
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

/// Derive a MASTER_KEY_LEN-byte password key (PK) from a password and salt using
/// Argon2id, with explicit cost parameters supplied by the caller (read
/// from that account's own user_salts row -- see module header on
/// parameter ownership).
///
/// password: raw bytes, not required to be valid UTF-8. Valid UTF-8 is
///   NFKC-normalised first (see the module note on unicode normalisation).
/// salt: any length argon2::Params/Argon2 itself accepts; SALT_LEN (16) is
///   this module's own generation length but is not enforced here, since a
///   caller reading an existing account's stored salt should not have this
///   function silently reject a salt it did not generate itself.
pub fn derive_password_key(
    password: &[u8],
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<[u8; MASTER_KEY_LEN], AuthError> {
    let params = Params::new(memory_kib, iterations, parallelism, Some(MASTER_KEY_LEN))
        .map_err(|e| AuthError::Derivation(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let password = normalise_password(password);
    let mut output = [0u8; MASTER_KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut output)
        .map_err(|e| AuthError::Derivation(e.to_string()))?;
    Ok(output)
}

/// derive_password_key on the blocking thread pool (items.id=630). Argon2id at
/// the default cost holds a core for a noticeable time and allocates 64 MiB;
/// called inline from an async fn it stalls every other task on that runtime
/// worker. The password is copied into a zeroizing buffer for the hop and the
/// returned key zeroizes on drop.
pub async fn derive_password_key_blocking(
    password: &[u8],
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<zeroize::Zeroizing<[u8; MASTER_KEY_LEN]>, AuthError> {
    let password = zeroize::Zeroizing::new(password.to_vec());
    let salt = salt.to_vec();
    tokio::task::spawn_blocking(move || {
        derive_password_key(&password, &salt, memory_kib, iterations, parallelism)
            .map(zeroize::Zeroizing::new)
    })
    .await
    .map_err(|e| AuthError::Derivation(format!("key derivation task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a single-threaded runtime an inline Argon2id call would starve every
    /// other task until it returned. A ticker task must keep running while the
    /// blocking helper derives.
    #[tokio::test(flavor = "current_thread")]
    async fn blocking_derivation_does_not_stall_the_runtime() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                t.fetch_add(1, Ordering::SeqCst);
            }
        });
        let salt = generate_salt().unwrap();
        // Heavy enough (64 MiB, 3 passes) to take tens of milliseconds.
        let key = derive_password_key_blocking(b"pw", &salt, 65536, 3, 4)
            .await
            .unwrap();
        let during = ticks.load(Ordering::SeqCst);
        ticker.abort();
        assert!(
            during >= 1,
            "the runtime made no progress during derivation"
        );
        assert_eq!(
            *key,
            derive_password_key(b"pw", &salt, 65536, 3, 4).unwrap(),
            "same result as the inline derivation"
        );
    }

    #[test]
    fn same_password_and_salt_yield_identical_key() {
        let salt = generate_salt().unwrap();
        let k1 = derive_password_key(b"correct horse battery staple", &salt, 1024, 1, 1).unwrap();
        let k2 = derive_password_key(b"correct horse battery staple", &salt, 1024, 1, 1).unwrap();
        assert_eq!(k1, k2, "same password+salt+params must derive identically");
    }

    #[test]
    fn different_salt_yields_different_key() {
        let salt_a = generate_salt().unwrap();
        let salt_b = generate_salt().unwrap();
        assert_ne!(salt_a, salt_b, "two generated salts should not collide");

        let k1 = derive_password_key(b"same password", &salt_a, 1024, 1, 1).unwrap();
        let k2 = derive_password_key(b"same password", &salt_b, 1024, 1, 1).unwrap();
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
        let key = derive_password_key(b"password", &salt, 1024, 1, 1).unwrap();
        assert_eq!(key.len(), MASTER_KEY_LEN);
    }

    #[test]
    fn invalid_parallelism_returns_error_not_panic() {
        let salt = generate_salt().unwrap();
        // p=0 is outside argon2::Params' valid range.
        let result = derive_password_key(b"password", &salt, 1024, 1, 0);
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
        let result = derive_password_key(b"password", &salt, 1, 3, 4);
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
        let key = derive_password_key(
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

    // -----------------------------------------------------------------------
    // Unicode normalisation (items.id=777 stage 0)
    // -----------------------------------------------------------------------

    fn hex(key: &[u8]) -> String {
        key.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn fixed_salt() -> Vec<u8> {
        (0u8..16).collect()
    }

    /// GOLDEN VECTORS captured from the derivation BEFORE normalisation was
    /// added (fixed salt 00..0f). ASCII, and non-ASCII text that is already
    /// NFKC, must keep deriving these exact keys, byte for byte, or every
    /// existing account would stop opening.
    #[test]
    fn keys_for_existing_passwords_are_unchanged() {
        let cases: [(&[u8], u32, u32, u32, &str); 7] = [
            (
                b"password123",
                1024,
                1,
                1,
                "4be78b7adf3077390f9593e33e6b719f96b2b3c111b97796b94eaa78e4651cba",
            ),
            (
                b"correct horse battery staple",
                1024,
                1,
                1,
                "f0d1525842159cef972608e2f8bc1cb2c8230f81eea76f124789f6beb42403b2",
            ),
            (
                b"  leading and trailing  ",
                1024,
                1,
                1,
                "0770f4702590af17a65c29f56df2eefee9ee813f97ca03a38802cacaceee750c",
            ),
            (
                b"",
                1024,
                1,
                1,
                "0a6f2a8fdc5ec4bf35827f17b098cb15026d0d990623f7232eea9b2d63a974d4",
            ),
            (
                b"P@ss w0rd! #1 ~ \"quoted\"",
                8192,
                2,
                2,
                "6c748172c9bb8ecdeb823835a33d714ba1b57fb6244ee88cc5c091d94ac8fa20",
            ),
            // Non-ASCII but already NFKC (composed e-acute and i-diaeresis, a
            // check mark, CJK): must not change either.
            (
                "na\u{ef}ve caf\u{e9} \u{2713} \u{65e5}\u{672c}\u{8a9e}".as_bytes(),
                1024,
                1,
                1,
                "11c07a1c52ba3851db260be5f8ccb3950a039d000e314fdb523c99a82ec1803c",
            ),
            (
                b"a real-shaped password",
                65536,
                3,
                4,
                "1d2b5f827d0802fe500ddb475d1c2768b2e62fff77d978cd9d16d0bb7b9fcd9a",
            ),
        ];
        for (pw, m, i, p, want) in cases {
            let got = derive_password_key(pw, &fixed_salt(), m, i, p).unwrap();
            assert_eq!(
                hex(&got),
                want,
                "key changed for {:?}",
                String::from_utf8_lossy(pw)
            );
        }
    }

    #[test]
    fn ascii_and_already_normal_text_is_not_copied() {
        assert!(matches!(
            normalise_password(b"plain ascii"),
            NormalisedPassword::Borrowed(_)
        ));
        assert!(matches!(
            normalise_password("caf\u{e9}".as_bytes()),
            NormalisedPassword::Borrowed(_)
        ));
    }

    #[test]
    fn full_width_and_ligature_derive_the_key_of_their_nfkc_form() {
        let salt = fixed_salt();
        let derive = |pw: &str| derive_password_key(pw.as_bytes(), &salt, 1024, 1, 1).unwrap();
        // Full-width letters and digits fold to ASCII.
        assert_eq!(
            derive("\u{ff50}\u{ff41}\u{ff53}\u{ff53}\u{ff11}\u{ff12}\u{ff13}"),
            derive("pass123")
        );
        // The "fi" ligature folds to the two letters.
        assert_eq!(derive("\u{fb01}nd the \u{fb01}sh"), derive("find the fish"));
        // Superscript two folds to a plain digit.
        assert_eq!(derive("x\u{b2}"), derive("x2"));
    }

    #[test]
    fn nfc_and_nfd_of_the_same_text_derive_the_same_key() {
        let salt = fixed_salt();
        let nfc = "caf\u{e9} cr\u{e8}me br\u{fb}l\u{e9}e \u{f1}";
        let nfd = "cafe\u{301} cre\u{300}me bru\u{302}le\u{301}e n\u{303}";
        assert_ne!(
            nfc.as_bytes(),
            nfd.as_bytes(),
            "the test needs two different encodings"
        );
        assert_eq!(
            derive_password_key(nfc.as_bytes(), &salt, 1024, 1, 1).unwrap(),
            derive_password_key(nfd.as_bytes(), &salt, 1024, 1, 1).unwrap()
        );
    }

    #[test]
    fn invalid_utf8_passes_through_unchanged() {
        // 0xff 0xfe is never valid UTF-8.
        let raw_bytes: Vec<u8> = vec![b'p', 0xff, 0xfe, b'w'];
        let raw: &[u8] = &raw_bytes;
        assert!(std::str::from_utf8(raw).is_err());
        assert!(matches!(normalise_password(raw), NormalisedPassword::Borrowed(b) if b == raw));
        // Borrowed means the bytes reach Argon2 exactly as given.
        let salt = fixed_salt();
        assert_eq!(
            derive_password_key(raw, &salt, 1024, 1, 1).unwrap(),
            derive_password_key(raw, &salt, 1024, 1, 1).unwrap()
        );
    }
}

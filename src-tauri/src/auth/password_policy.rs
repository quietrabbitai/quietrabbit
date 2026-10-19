// src-tauri/src/auth/password_policy.rs
//
// The account password policy of decisions.id=858 (items.id=777). Pure: no IO,
// no state, no secret ever leaves this module (the only output is a coded
// verdict). items.id=637's create_account calls the same functions; nothing
// here knows which screen asked.
//
//   - minimum 15 characters, no upper cap (D3), spaces and passphrases allowed,
//     no composition rules;
//   - common and breached passwords blocked, against a pinned LOCAL list
//     compiled into the binary (password_blocklist.txt, see its MANIFEST). No
//     network lookup of any kind.
//
// NORMALISATION (decisions.id=900): the rules run on the NFKC form of what was
// typed, the same form kdf::derive_password_key hashes, so "what counts as 15
// characters", "what is on the list" and "is this the current password" are
// all judged on exactly the text that becomes the key. Length is counted in
// Unicode code points of that normalised text (not bytes, not UTF-16 units).
// The frontend count uses String.prototype.normalize('NFKC') and is advisory;
// this module is authoritative.
//
// Nothing is trimmed: leading and trailing spaces are part of a password. The
// blocklist lookup alone also tries a trimmed and a whitespace-free copy of the
// candidate, so padding a famous string with spaces does not slip past it.
//
// WHERE IT DOES NOT APPLY: only a password being CHOSEN is checked. Sign-in,
// resuming an interrupted change, and cancelling one (which types the new
// password that was already chosen) never call this, so an account whose
// password predates the policy can always sign in and always cancel.

use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

/// decisions.id=858. Mirrored by MIN_PASSWORD_LENGTH in
/// frontend/src/auth/passwordRules.ts.
pub const MIN_PASSWORD_LENGTH: usize = 15;

/// Sorted, lowercase, NFKC, one entry per line, every entry at least
/// MIN_PASSWORD_LENGTH code points. Built by tools/build_password_blocklist.py.
static BLOCKLIST_TEXT: &str = include_str!("password_blocklist.txt");

/// Why a new password is not acceptable. Carries nothing about the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordPolicyError {
    /// Nothing was typed.
    Empty,
    /// Fewer than MIN_PASSWORD_LENGTH characters.
    TooShort,
    /// On the common / breached list.
    Blocked,
}

/// The NFKC form of `password`, zeroized on drop.
fn nfkc(password: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(password.len()));
    out.extend(password.nfkc());
    out
}

/// Length in Unicode code points of the NFKC form.
pub fn password_length(password: &str) -> usize {
    nfkc(password).chars().count()
}

/// True when both strings are the same password: equal after NFKC, which is
/// exactly when kdf::derive_password_key derives the same key for them.
pub fn same_password(a: &str, b: &str) -> bool {
    *nfkc(a) == *nfkc(b)
}

fn blocklist() -> &'static [&'static str] {
    static LIST: OnceLock<Vec<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| BLOCKLIST_TEXT.lines().collect())
}

fn listed(entry: &str) -> bool {
    blocklist().binary_search(&entry).is_ok()
}

/// True when the password is on the common / breached list.
pub fn is_blocklisted(password: &str) -> bool {
    let lower = Zeroizing::new(nfkc(password).to_lowercase());
    if listed(&lower) || listed(lower.trim()) {
        return true;
    }
    let spaceless: Zeroizing<String> =
        Zeroizing::new(lower.chars().filter(|c| !c.is_whitespace()).collect());
    listed(&spaceless)
}

/// Check a password that is about to be chosen. Order: empty, too short,
/// blocklisted. The "same as the current password" rule is not here: it needs
/// the verified current password and lives in change_password.
pub fn validate_new_password(password: &str) -> Result<(), PasswordPolicyError> {
    if password.is_empty() {
        return Err(PasswordPolicyError::Empty);
    }
    if password_length(password) < MIN_PASSWORD_LENGTH {
        return Err(PasswordPolicyError::TooShort);
    }
    if is_blocklisted(password) {
        return Err(PasswordPolicyError::Blocked);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// SHA-256 of password_blocklist.txt as committed (also in the MANIFEST).
    /// Changing the list is deliberate: regenerate with the tool, update this
    /// constant and the MANIFEST together.
    const BLOCKLIST_SHA256: &str =
        "f305f8d7c39fdc57e3b32189e21e9efc9e75f5e9ef05c5584b111fb8b8efa4f9";

    // These are real entries of the pinned list (public, breach-derived).
    const LISTED: &str = "123456789daniel";
    const LISTED_2: &str = "samsung123456789";

    #[test]
    fn blocklist_file_is_the_pinned_one() {
        let digest = Sha256::digest(BLOCKLIST_TEXT.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, BLOCKLIST_SHA256);
    }

    #[test]
    fn blocklist_entries_are_well_formed_and_sorted() {
        let list = blocklist();
        assert!(list.len() > 1000, "the list looks truncated");
        for pair in list.windows(2) {
            assert!(pair[0] < pair[1], "not sorted or duplicated: {:?}", pair);
        }
        for entry in list {
            assert!(
                entry.chars().count() >= MIN_PASSWORD_LENGTH,
                "shorter than the minimum, can never match: {entry:?}"
            );
            assert_eq!(*entry, entry.to_lowercase(), "not lowercase: {entry:?}");
            assert_eq!(
                *entry,
                entry.nfkc().collect::<String>(),
                "not NFKC: {entry:?}"
            );
        }
    }

    #[test]
    fn length_is_counted_in_code_points_after_normalisation() {
        assert_eq!(password_length(""), 0);
        assert_eq!(password_length("abc"), 3);
        // Composed and decomposed forms of the same text count the same.
        assert_eq!(password_length("caf\u{e9}"), 4);
        assert_eq!(password_length("cafe\u{301}"), 4);
        // Not bytes: 5 CJK characters are 15 bytes.
        assert_eq!(
            password_length("\u{65e5}\u{672c}\u{8a9e}\u{6f22}\u{5b57}"),
            5
        );
        // Not UTF-16 units: an astral character is one code point.
        assert_eq!(password_length("\u{1f600}"), 1);
        // Normalisation can change the count: the "ffi" ligature is 3 letters,
        // full-width digits are 1 each.
        assert_eq!(password_length("\u{fb03}"), 3);
        assert_eq!(password_length("\u{ff11}\u{ff12}"), 2);
    }

    #[test]
    fn the_minimum_is_fifteen() {
        assert_eq!(MIN_PASSWORD_LENGTH, 15);
        let fourteen = "a1b2c3d4e5f6g7";
        let fifteen = "a1b2c3d4e5f6g7h";
        assert_eq!(
            validate_new_password(fourteen),
            Err(PasswordPolicyError::TooShort)
        );
        assert_eq!(validate_new_password(fifteen), Ok(()));
    }

    #[test]
    fn empty_is_its_own_verdict() {
        assert_eq!(validate_new_password(""), Err(PasswordPolicyError::Empty));
    }

    #[test]
    fn fifteen_characters_that_are_not_fifteen_bytes() {
        // 15 emoji = 15 code points (60 bytes, 30 UTF-16 units): accepted.
        let emoji = "\u{1f600}".repeat(15);
        assert_eq!(validate_new_password(&emoji), Ok(()));
        // 8 CJK characters are 24 bytes but only 8 characters: too short.
        let cjk = "\u{65e5}\u{672c}\u{8a9e}\u{6f22}\u{5b57}\u{4eba}\u{6c11}\u{6c34}";
        assert_eq!(
            validate_new_password(cjk),
            Err(PasswordPolicyError::TooShort)
        );
    }

    #[test]
    fn length_is_judged_after_normalisation() {
        // 15 full-width letters are 15 characters either way...
        let wide: String = "\u{ff58}\u{ff51}\u{ff5a}\u{ff4a}\u{ff57}\u{ff4b}\u{ff56}\u{ff47}\u{ff42}\u{ff4d}\u{ff46}\u{ff59}\u{ff50}\u{ff55}\u{ff48}".to_owned();
        assert_eq!(wide.chars().count(), 15);
        assert_eq!(validate_new_password(&wide), Ok(()));
        // ...but 5 "ffi" ligatures (5 typed characters) fold to 15 letters.
        let ligatures = "\u{fb03}".repeat(5);
        assert_eq!(validate_new_password(&ligatures), Ok(()));
        // And a decomposed accent is one character, not two.
        let decomposed = "e\u{301}".repeat(14);
        assert_eq!(
            validate_new_password(&decomposed),
            Err(PasswordPolicyError::TooShort)
        );
    }

    #[test]
    fn long_passphrases_with_spaces_are_accepted() {
        assert_eq!(validate_new_password("gentle river stone lantern"), Ok(()));
        let sixty_four = "x7Qp ".repeat(13); // 65 characters
        assert!(password_length(&sixty_four) >= 64);
        assert_eq!(validate_new_password(&sixty_four), Ok(()));
        // No upper cap (D3).
        let very_long = "gentle river stone lantern ".repeat(200);
        assert!(password_length(&very_long) > 5000);
        assert_eq!(validate_new_password(&very_long), Ok(()));
    }

    #[test]
    fn a_famous_passphrase_is_still_blocked() {
        // Spaces are allowed, but this exact phrase is on the list.
        assert_eq!(
            validate_new_password("correct horse battery staple"),
            Err(PasswordPolicyError::Blocked)
        );
        assert_eq!(
            validate_new_password("abcdefghijklmno"),
            Err(PasswordPolicyError::Blocked)
        );
    }

    #[test]
    fn no_composition_rules() {
        assert_eq!(validate_new_password("alllowercaseletters"), Ok(()));
        assert_eq!(validate_new_password("   fifteen spaces!   "), Ok(()));
    }

    #[test]
    fn listed_passwords_are_blocked_case_insensitively_and_padded() {
        assert!(listed(LISTED));
        assert!(listed(LISTED_2));
        assert_eq!(
            validate_new_password(LISTED),
            Err(PasswordPolicyError::Blocked)
        );
        assert_eq!(
            validate_new_password(&LISTED.to_uppercase()),
            Err(PasswordPolicyError::Blocked)
        );
        assert_eq!(
            validate_new_password(&format!("   {LISTED}  ")),
            Err(PasswordPolicyError::Blocked)
        );
        // Spaces inserted inside a listed string do not hide it.
        assert_eq!(
            validate_new_password("123456789 daniel"),
            Err(PasswordPolicyError::Blocked)
        );
        // Full-width and ligature forms fold to the listed text first.
        assert_eq!(
            validate_new_password("\u{ff11}\u{ff12}\u{ff13}456789daniel"),
            Err(PasswordPolicyError::Blocked)
        );
    }

    #[test]
    fn unlisted_long_passwords_are_not_blocked() {
        assert!(!is_blocklisted("a different password"));
        assert!(!is_blocklisted("Zw\u{f6}lf Boxk\u{e4}mpfer jagen Viktor"));
    }

    #[test]
    fn same_password_compares_the_normalised_form() {
        assert!(same_password(
            "correct horse battery",
            "correct horse battery"
        ));
        assert!(same_password(
            "caf\u{e9} au lait ok",
            "cafe\u{301} au lait ok"
        ));
        assert!(same_password("\u{ff21}bc", "Abc"));
        assert!(!same_password("Abc", "abc"));
        assert!(!same_password("abc", "abc "));
    }
}

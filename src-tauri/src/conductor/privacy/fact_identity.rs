// src-tauri/src/conductor/privacy/fact_identity.rs
//
// items.id=406 (decisions.id=756/757) -- Privacy Guardian persistence
// cascade, Layer 1: stable content-hash identity.
//
// Canonicalizes a detected PF span's resolved value per category, then
// hashes the (category, canonical) pair so a re-detected identical fact
// produces the same ID regardless of surrounding-text reword. Deterministic
// only -- no fuzzy/similarity matching anywhere in this module (embeddings
// are explicitly out of scope as a decision mechanism, decisions.id=757).
//
// Buildable for 6 of 8 PF taxonomy categories -- bounded, per-category
// normalization (items.id=409 finding, confirmed accepted 2026-09-03):
//   private_email, private_phone, private_url, private_date,
//   account_number, secret
// private_person and private_address resist trivial normalization and are
// NOT attempted here -- canonicalize_fact returns None for both, by design,
// not as a missing case. Callers fall back to Layer 2 (within-conversation
// coreference, coref.rs) or Layer 3 (entity_facts resolution) for those two
// categories, or re-ask if neither resolves either.
//
// Hash format follows the existing PersonalField::compute_content_hash
// precedent (conductor/types.rs): SHA-256 of "category:canonical", hex
// digest. Same delimiter convention, same crate (sha2, already a
// dependency) -- no new hashing approach introduced for this one case.

use sha2::{Digest, Sha256};

/// Canonicalizes a detected span's raw text for the given PF category.
/// Returns `None` for `private_person`/`private_address`/any unrecognized
/// category -- these never get a Layer 1 hash (see module doc).
pub fn canonicalize_fact(category: &str, raw_text: &str) -> Option<String> {
    match category {
        "private_email" => Some(raw_text.trim().to_lowercase()),
        "private_phone" => {
            let digits: String = raw_text.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                None
            } else {
                Some(digits)
            }
        }
        "private_url" => canonicalize_url(raw_text),
        "private_date" => canonicalize_date(raw_text),
        "account_number" => {
            let cleaned: String = raw_text
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '-')
                .collect::<String>()
                .to_uppercase();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        }
        // secret: exact-match-only (items.id=409 finding) -- trim only, no
        // other normalization. A secret's exact form is load-bearing.
        "secret" => {
            let trimmed = raw_text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        // private_person / private_address / unrecognized: no Layer 1 hash.
        _ => None,
    }
}

/// Lowercase scheme+host, strip a trailing slash. Deliberately not a full
/// URL-normalization RFC pass (query-param ordering, punycode, etc.) --
/// "bounded" per items.id=409, not exhaustive. `None` only if the input is
/// empty after trimming; anything else is normalized best-effort, since a
/// URL's path/query casing is often meaningful and shouldn't be lowercased
/// wholesale.
fn canonicalize_url(raw_text: &str) -> Option<String> {
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_lowercase();
    let stripped = lowered.strip_suffix('/').unwrap_or(&lowered);
    Some(stripped.to_owned())
}

/// Parses a couple of common date formats to ISO-8601 (YYYY-MM-DD).
/// `None` if unparseable -- honest rather than a mediocre partial
/// normalization (items.id=406 planning note). Supports:
///   YYYY-MM-DD (already canonical)
///   MM/DD/YYYY
///   M/D/YYYY
fn canonicalize_date(raw_text: &str) -> Option<String> {
    let trimmed = raw_text.trim();

    // Already ISO-8601.
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() == 3
        && parts[0].len() == 4
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        let (y, m, d): (u32, u32, u32) = (
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        );
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            return Some(format!("{y:04}-{m:02}-{d:02}"));
        }
        return None;
    }

    // MM/DD/YYYY or M/D/YYYY.
    let slash_parts: Vec<&str> = trimmed.split('/').collect();
    if slash_parts.len() == 3 {
        let (m, d, y): (u32, u32, u32) = (
            slash_parts[0].parse().ok()?,
            slash_parts[1].parse().ok()?,
            slash_parts[2].parse().ok()?,
        );
        if (1..=12).contains(&m) && (1..=31).contains(&d) && y >= 1000 {
            return Some(format!("{y:04}-{m:02}-{d:02}"));
        }
    }

    None
}

/// SHA-256 of "category:canonical", hex digest. Follows the same
/// delimiter/format convention as PersonalField::compute_content_hash
/// (conductor/types.rs) -- not a new hashing approach.
pub fn fact_hash(category: &str, canonical: &str) -> String {
    let payload = format!("{category}:{canonical}");
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Layer 3: entity_facts resolution
// ---------------------------------------------------------------------------
//
// items.id=409's finding: no PF-category-to-entity_facts.field_name mapping
// exists, and building one safely is genuinely new, hard work -- not
// attempted here. Instead: a direct VALUE match. If a detected span's text
// (or its Layer 1 canonical form, for the 6 hashable categories) equals an
// existing entity_facts.field_value for this persona, that row's stable id
// -- durable, encrypted, already persisted independent of this feature --
// IS the cross-conversation identity. No fuzzy/similarity matching (out of
// scope everywhere in this cascade, decisions.id=757). A persona's
// entity_facts count is small; a linear scan per gate3 call is not a
// performance concern (gate3 already budgets a 10s FFI call).

/// True if `field_value` (an existing entity_facts row's decrypted value)
/// represents the same fact as a span's `original_text` in the given
/// `category`. Pure matching logic, split out from the DB read so it's
/// unit-testable without a live encrypted personal.db.
fn matches_entity_fact(category: &str, original_text: &str, field_value: &str) -> bool {
    match canonicalize_fact(category, original_text) {
        Some(canon) => canonicalize_fact(category, field_value).as_deref() == Some(canon.as_str()),
        // private_person / private_address / unrecognized: exact-text match only.
        None => original_text.trim() == field_value.trim(),
    }
}

/// Resolves a detected span against this persona's entity_facts store.
/// Returns `Some("entity_facts:{id}")` on a match -- a stable identity
/// usable for BOTH conversation-scoped and Persona-scoped (opt-in standing
/// preference) persistence, since entity_facts rows already persist across
/// conversations. `None` if nothing matches (falls through to "ask the
/// user", unchanged fallback per decisions.id=757).
pub async fn resolve_against_entity_facts(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    category: &str,
    original_text: &str,
) -> Result<Option<String>, crate::persistence::personal_store::PersonalStoreError> {
    let facts = crate::persistence::personal_store::load_entity_facts_for_context(
        user_id, persona_id, key_hex,
    )
    .await?;

    Ok(facts
        .into_iter()
        .find(|fact| matches_entity_fact(category, original_text, &fact.field_value))
        .map(|fact| format!("entity_facts:{}", fact.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- canonicalize_fact: hashable categories ------------------------------

    #[test]
    fn email_is_lowercased_and_trimmed() {
        assert_eq!(
            canonicalize_fact("private_email", "  John@Example.com "),
            Some("john@example.com".to_owned())
        );
    }

    #[test]
    fn phone_strips_non_digits() {
        assert_eq!(
            canonicalize_fact("private_phone", "(555) 123-4567"),
            Some("5551234567".to_owned())
        );
    }

    #[test]
    fn phone_with_country_code_and_plus() {
        assert_eq!(
            canonicalize_fact("private_phone", "+1 555-123-4567"),
            Some("15551234567".to_owned())
        );
    }

    #[test]
    fn url_is_lowercased_with_trailing_slash_stripped() {
        assert_eq!(
            canonicalize_fact("private_url", "HTTPS://Example.com/Profile/"),
            Some("https://example.com/profile".to_owned())
        );
    }

    #[test]
    fn date_mm_dd_yyyy_normalizes_to_iso() {
        assert_eq!(
            canonicalize_fact("private_date", "3/5/2026"),
            Some("2026-03-05".to_owned())
        );
    }

    #[test]
    fn date_already_iso_passes_through() {
        assert_eq!(
            canonicalize_fact("private_date", "2026-03-05"),
            Some("2026-03-05".to_owned())
        );
    }

    #[test]
    fn date_unparseable_returns_none() {
        assert_eq!(canonicalize_fact("private_date", "next Thursday"), None);
    }

    #[test]
    fn account_number_strips_whitespace_and_dashes_and_uppercases() {
        assert_eq!(
            canonicalize_fact("account_number", "ab-12 34-cd"),
            Some("AB1234CD".to_owned())
        );
    }

    #[test]
    fn secret_is_trim_only_exact_match() {
        assert_eq!(
            canonicalize_fact("secret", "  sk-abc123XYZ  "),
            Some("sk-abc123XYZ".to_owned())
        );
        // Case is NOT normalized for secrets -- exact match only.
        assert_ne!(
            canonicalize_fact("secret", "SK-ABC123XYZ"),
            canonicalize_fact("secret", "sk-abc123xyz")
        );
    }

    // -- canonicalize_fact: non-hashable categories --------------------------

    #[test]
    fn private_person_has_no_layer_one_hash() {
        assert_eq!(canonicalize_fact("private_person", "Jane Doe"), None);
    }

    #[test]
    fn private_address_has_no_layer_one_hash() {
        assert_eq!(canonicalize_fact("private_address", "123 Main St"), None);
    }

    #[test]
    fn unrecognized_category_has_no_layer_one_hash() {
        assert_eq!(canonicalize_fact("private_ssn", "123-45-6789"), None);
    }

    // -- fact_hash: stability across superficial input differences ----------

    #[test]
    fn hash_stable_across_case_and_whitespace_for_email() {
        let a = canonicalize_fact("private_email", "John@Example.com").unwrap();
        let b = canonicalize_fact("private_email", " john@example.com ").unwrap();
        assert_eq!(
            fact_hash("private_email", &a),
            fact_hash("private_email", &b)
        );
    }

    #[test]
    fn hash_differs_across_categories_for_the_same_canonical_text() {
        // "category:canonical" delimiter means the category is part of the
        // hashed payload -- two different categories with coincidentally
        // identical canonical text must not collide.
        let h1 = fact_hash("private_email", "12345");
        let h2 = fact_hash("account_number", "12345");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_values_same_category() {
        let h1 = fact_hash("private_email", "a@example.com");
        let h2 = fact_hash("private_email", "b@example.com");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(
            fact_hash("private_email", "a@example.com"),
            fact_hash("private_email", "a@example.com")
        );
    }

    // -- matches_entity_fact (Layer 3, DB-free matching logic) ---------------

    #[test]
    fn hashable_category_matches_via_canonicalization() {
        assert!(matches_entity_fact(
            "private_email",
            "John@Example.com",
            "john@example.com"
        ));
    }

    #[test]
    fn hashable_category_does_not_match_a_different_value() {
        assert!(!matches_entity_fact(
            "private_email",
            "john@example.com",
            "someone.else@example.com"
        ));
    }

    #[test]
    fn non_hashable_category_falls_back_to_exact_trimmed_text_match() {
        assert!(matches_entity_fact(
            "private_person",
            "  Jane Doe ",
            "Jane Doe"
        ));
        assert!(!matches_entity_fact(
            "private_person",
            "Jane Doe",
            "Jane D."
        ));
    }

    #[test]
    fn phone_matches_across_punctuation_differences() {
        assert!(matches_entity_fact(
            "private_phone",
            "(555) 123-4567",
            "555.123.4567"
        ));
    }
}

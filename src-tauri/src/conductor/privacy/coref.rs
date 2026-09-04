// src-tauri/src/conductor/privacy/coref.rs
//
// items.id=406 (decisions.id=756/757) -- Privacy Guardian persistence
// cascade, Layer 2: within-conversation coreference.
//
// Scoped strictly to "was this fact already decided earlier in THIS SAME
// conversation" (focus_run_id) -- not cross-conversation (that's Layer 3,
// entity_facts resolution). Primarily the fallback for private_person/
// private_address, which Layer 1 (fact_identity.rs) never hashes, but also
// usable to link a reworded mention of an otherwise-hashable fact within
// one conversation.
//
// Backed by the `anno` crate's rule-based SimpleCorefResolver (pinned
// version =0.11.0, `default-features = false`) -- confirmed by direct
// source read (backends/coref/mod.rs) that SimpleCorefResolver
// (backends/coref/simple.rs) carries NO #[cfg(feature = ...)] gate of its
// own; only the separate ONNX-based resolver in that module is
// feature-gated. No ONNX/model download anywhere in this dependency chain
// with default-features=false (confirmed: `cargo add anno@=0.11.0
// --no-default-features` pulls in only dirs/dirs-sys/itertools/
// redox_users/textprep -- no candle/tokenizers/ort).
//
// PER ITEMS.ID=406'S BUILD INSTRUCTION: this crate is young (858 downloads
// per items.id=409's finding) -- tests are written FIRST, in this same
// file, before any wiring into gate3.rs. Only once these pass does this
// module get called from gate3_with_pf.
//
// WHY A SYNTHETIC JOINED TEXT: SimpleCorefResolver's sieves operate over
// entities assumed to share one real document (character-proximity-based
// appositive detection, i-within-i nesting guards). Prior mentions here
// come from SEPARATE gate3 invocations -- different content_text each time
// -- so there is no real shared document to anchor character offsets to.
// Building one wide synthetic text (each mention's real text, joined by a
// long fixed separator) gives every entity a distinct, non-overlapping
// slot: proximity/nesting sieves can never misfire on fabricated adjacency
// (the separator is far longer than the appositive-detection gap), while
// the crate's genuinely text-driven sieves (exact/head/containment/fuzzy
// match) and pronoun lookback (an entity-count window, not a character
// distance) remain meaningful and are what this layer actually relies on.

use anno::backends::coref::{CorefConfig, SimpleCorefResolver};
use anno::{Entity, EntityCategory, EntityType};

/// One previously-seen span mention in the current conversation
/// (focus_run_id-scoped), as read back from the pf_fact_mentions table.
#[derive(Debug, Clone)]
pub struct PfFactMention {
    pub category: String,
    pub fact_key: String,
    pub original_text: String,
}

/// Long, deliberately non-meaningful separator -- see module doc for why
/// this must exceed SimpleCorefResolver's appositive-detection gap (<= 2
/// chars) so synthetic adjacency can never be mistaken for a real one.
const SYNTHETIC_JOIN_SEPARATOR: &str = "\n\n----\n\n";

/// Maps a PF taxonomy category to anno's EntityType, for coreference
/// matching purposes only -- this is NOT the same mapping problem items.id=
/// 409 found doesn't exist for entity_facts.field_name (Layer 3); this one
/// only needs to pick a reasonable type/gender-agreement bucket for
/// SimpleCorefResolver's sieves, not a durable schema-level relationship.
fn entity_type_for_category(category: &str) -> EntityType {
    match category {
        "private_person" => EntityType::Person,
        "private_address" => EntityType::Location,
        "private_email" => EntityType::Email,
        "private_phone" => EntityType::Phone,
        "private_url" => EntityType::Url,
        "private_date" => EntityType::Date,
        other => EntityType::Custom {
            name: other.to_owned(),
            category: EntityCategory::Misc,
        },
    }
}

/// Resolves whether `new_span_text` (category `new_category`) corefers with
/// any of `prior_mentions` -- spans already seen earlier in this same
/// conversation. Returns the `fact_key` to reuse if so; `None` if this
/// looks like a genuinely new fact (including when `prior_mentions` is
/// empty). A miss here only means a within-conversation re-ask, never a
/// silent cross-conversation leak -- Privacy Filter's own detection is
/// entirely unaffected by this layer's outcome either way.
pub fn resolve_within_conversation(
    prior_mentions: &[PfFactMention],
    new_category: &str,
    new_span_text: &str,
) -> Option<String> {
    if prior_mentions.is_empty() {
        return None;
    }

    let resolver = SimpleCorefResolver::new(CorefConfig::default());

    let mut entities = Vec::with_capacity(prior_mentions.len() + 1);
    let mut cursor = 0usize;
    for mention in prior_mentions {
        let start = cursor;
        let end = start + mention.original_text.len();
        entities.push(Entity::new(
            mention.original_text.clone(),
            entity_type_for_category(&mention.category),
            start,
            end,
            1.0,
        ));
        cursor = end + SYNTHETIC_JOIN_SEPARATOR.len();
    }
    let new_start = cursor;
    let new_end = new_start + new_span_text.len();
    entities.push(Entity::new(
        new_span_text.to_owned(),
        entity_type_for_category(new_category),
        new_start,
        new_end,
        1.0,
    ));

    let resolved = resolver.resolve(&entities);
    let new_canonical_id = resolved.last()?.canonical_id?;

    resolved[..resolved.len() - 1]
        .iter()
        .zip(prior_mentions.iter())
        .find(|(entity, _)| entity.canonical_id == Some(new_canonical_id))
        .map(|(_, mention)| mention.fact_key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(category: &str, fact_key: &str, text: &str) -> PfFactMention {
        PfFactMention {
            category: category.to_owned(),
            fact_key: fact_key.to_owned(),
            original_text: text.to_owned(),
        }
    }

    #[test]
    fn empty_prior_mentions_never_resolves() {
        assert_eq!(
            resolve_within_conversation(&[], "private_person", "Jane Doe"),
            None
        );
    }

    #[test]
    fn verbatim_repeat_resolves_as_same() {
        let prior = vec![mention("private_person", "fact-1", "Jane Doe")];
        assert_eq!(
            resolve_within_conversation(&prior, "private_person", "Jane Doe"),
            Some("fact-1".to_owned())
        );
    }

    #[test]
    fn verbatim_repeat_is_case_insensitive() {
        // SimpleCorefResolver's exact-canonical-match sieve lowercases before
        // comparing (canonical_form()) -- confirm that behavior surfaces
        // through this wrapper, not just documented in the crate itself.
        let prior = vec![mention("private_email", "fact-1", "Jane@Example.com")];
        assert_eq!(
            resolve_within_conversation(&prior, "private_email", "jane@example.com"),
            Some("fact-1".to_owned())
        );
    }

    #[test]
    fn pronoun_reference_to_prior_name_resolves_as_same() {
        let prior = vec![mention("private_person", "fact-1", "John Smith")];
        assert_eq!(
            resolve_within_conversation(&prior, "private_person", "he"),
            Some("fact-1".to_owned())
        );
    }

    #[test]
    fn two_distinct_names_do_not_resolve() {
        let prior = vec![mention("private_person", "fact-1", "Jane Doe")];
        assert_eq!(
            resolve_within_conversation(&prior, "private_person", "Bob Johnson"),
            None
        );
    }

    #[test]
    fn two_distinct_categories_with_the_same_text_do_not_collide() {
        // entity_type differs (Person vs a Custom "secret" bucket) -- the
        // exact-canonical-match sieve's canonical form includes the type
        // label, so identical text under different categories must not merge.
        let prior = vec![mention("private_person", "fact-1", "Phoenix")];
        assert_eq!(
            resolve_within_conversation(&prior, "secret", "Phoenix"),
            None
        );
    }

    #[test]
    fn resolves_against_the_correct_one_of_several_prior_mentions() {
        let prior = vec![
            mention("private_person", "fact-1", "Jane Doe"),
            mention("private_email", "fact-2", "jane@example.com"),
            mention("private_person", "fact-3", "Bob Johnson"),
        ];
        assert_eq!(
            resolve_within_conversation(&prior, "private_person", "Jane Doe"),
            Some("fact-1".to_owned())
        );
        assert_eq!(
            resolve_within_conversation(&prior, "private_email", "JANE@EXAMPLE.COM"),
            Some("fact-2".to_owned())
        );
    }
}

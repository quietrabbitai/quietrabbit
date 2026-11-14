// src-tauri/src/conductor/collection_consent_text.rs
//
// items.id=920 stage 2 (decisions.id=944): the VERSION and the PROMISE IDS of the
// one-time collection-consent text, as data (src-tauri/block_data/
// collection_consent_text.json, the document_types.json precedent). Rust holds no
// consent wording at all. The user-visible prose is placeholder i18n copy in the
// frontend, owned by Chat-BRAND (items.id=839); it cites a promise by its id.
//
// WHY A VERSION. The gate opens only for a grant of the CURRENT version
// (focus_settings_store::CollectionConsentState::is_current_grant), and a decision
// recorded for an older version is asked again. Changing what the text promises
// means bumping text_version in the data file, which forces a new decision from
// every Persona. A test pins the shipped version so a bump is deliberate.
//
// WHICH PROMISES. Only a promise QR keeps (or ships before Tech Support can be
// enabled, see PROMISES) may be listed, so the file may
// name only ids in PROMISES (a closed list, validated at first use by the same
// function the test calls). Deliberately NOT promised, because they are not
// built: editing reference sheets, deleting a single case, delta or output, and
// use of a Focus's entities by another Focus (decisions.id=509: no cross-Focus
// use). Adding a promise id is a code change plus a text_version bump.

use std::sync::OnceLock;

use serde::Deserialize;

/// The closed list of promises the text may cite. Each is built, or ships before
/// Tech Support can be enabled: delete_device_and_linked_data depends on the
/// items.id=891 purge and the items.id=888 link cleanup (decisions.id=940).
///   collects_listed_categories_only        the Focus saves only the categories the
///                                          text lists (the writer ops' typed slots)
///   never_shared_outside_qr_without_review stays in QR's local stores and never
///                                          reaches a Tier 2 or 3 context unreviewed
///   per_session_opt_out                    the one-time latch (decisions.id=509)
///   withdraw_any_time                      withdraw_collection_consent
///   delete_device_and_linked_data          deleting a device and everything linked
///                                          to it
pub const PROMISES: [&str; 5] = [
    "collects_listed_categories_only",
    "never_shared_outside_qr_without_review",
    "per_session_opt_out",
    "withdraw_any_time",
    "delete_device_and_linked_data",
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentText {
    pub text_version: i64,
    pub promises: Vec<String>,
}

impl ConsentText {
    pub fn validate(&self) -> Result<(), String> {
        if self.text_version < 1 {
            return Err("text_version must be 1 or more".to_owned());
        }
        if self.promises.is_empty() {
            return Err("no promises listed".to_owned());
        }
        for (i, p) in self.promises.iter().enumerate() {
            if !PROMISES.contains(&p.as_str()) {
                return Err(format!("promise '{p}' is not a built promise"));
            }
            if self.promises[..i].contains(p) {
                return Err(format!("promise '{p}' is listed twice"));
            }
        }
        Ok(())
    }
}

const BUNDLED: &str = include_str!("../../block_data/collection_consent_text.json");

/// Parse and validate the bundled data once. An Err is a build defect (a test
/// parses the same file): the commands report it and the gate, which compares
/// against current_version(), treats it as no current version, so nothing opens.
pub fn registry() -> Result<&'static ConsentText, String> {
    static REGISTRY: OnceLock<Result<ConsentText, String>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let t: ConsentText = serde_json::from_str(BUNDLED)
                .map_err(|e| format!("block_data/collection_consent_text.json: {e}"))?;
            t.validate()
                .map_err(|e| format!("block_data/collection_consent_text.json: {e}"))?;
            Ok(t)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The version of the text a Persona must have agreed to. 0 when the data is
/// unusable: no stored version is 0 for a grant, so nothing can match it and the
/// gate stays closed.
pub fn current_version() -> i64 {
    registry().map(|t| t.text_version).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_text_data_validates_and_is_pinned() {
        let t = registry().expect("the bundled consent text must validate");
        // Bumping the version is a deliberate act that forces a new decision from
        // every Persona: change this number with the data file.
        assert_eq!(t.text_version, 1);
        assert_eq!(current_version(), 1);
        assert_eq!(t.promises, PROMISES.to_vec(), "exactly the built promises");
    }

    #[test]
    fn only_built_promises_can_be_listed() {
        let ok = |p: &[&str]| ConsentText {
            text_version: 1,
            promises: p.iter().map(|s| (*s).to_owned()).collect(),
        };
        assert!(ok(&PROMISES).validate().is_ok());
        for not_built in [
            "edit_reference_sheets",
            "delete_individual_cases",
            "delete_individual_outputs",
            "cross_focus_entity_use",
        ] {
            assert!(ok(&[not_built]).validate().is_err(), "{not_built}");
        }
        assert!(ok(&[]).validate().is_err());
        assert!(ok(&["withdraw_any_time", "withdraw_any_time"])
            .validate()
            .is_err());
        assert!(ConsentText {
            text_version: 0,
            ..ok(&PROMISES)
        }
        .validate()
        .is_err());
    }

    #[test]
    fn the_data_file_carries_no_prose() {
        // Ids and a version only: unknown keys are refused, and every promise id
        // is a short snake_case token, not a sentence.
        assert!(serde_json::from_str::<ConsentText>(
            r#"{"text_version":1,"promises":["withdraw_any_time"],"title":"Hello"}"#
        )
        .is_err());
        for p in PROMISES {
            assert!(p.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{p}");
        }
    }
}

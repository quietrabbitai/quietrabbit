// src-tauri/src/conductor/device_fields.rs
//
// items.id=891 stage 2 (decisions.id=507, 509, 940): the CLOSED CATALOGUE of the
// facts a Focus may save about a device it owns, as data
// (src-tauri/block_data/device_fields.json, the collection_consent_text.json and
// document_types.json precedent). Rust holds the validation rules and no field
// names; adding or tuning a field is a data edit that this module validates.
//
// WHY A CLOSED LIST. The entity_fact_put writer takes the FIELD NAME from a model
// output (a variable), so the catalogue is what stops a model from inventing a
// field, and above all from writing a credential: decisions.id=507's credentials
// invariant says QR never extracts, stages or writes a password, PIN, passphrase
// or token to entity_facts, whatever the consent or posture. The catalogue has no
// such field and no free-text note field beyond the one distilled known_issues
// entry, and validate() refuses any field NAME that contains a credential word, so
// a later data edit cannot add one by accident. An unknown field fails the pass
// closed (the items.id=887 precedent for an invalid model value). There is no
// secret-shape detector on values (Jason's ruling for items.id=891): the closed
// list and the length cap are the guard.
//
// WHAT A FIELD CARRIES: its category (decisions.id=507's categories, a closed list
// here), a label (the wording a derived text uses, never a {token}), the
// sensitivity, its DISCLOSURE (items.id=924: never, ask or pass, what may reach a
// hosted step) with the generalization a review suggests for an ask field, a byte
// cap, and an optional access_credential_flag (decisions.id=475; no shipped field keeps it,
// the serial number lost it to decisions.id=956, items.id=936).
//
// DISCLOSURE (items.id=924, decisions.id=507 credentials invariant kept). never: the
// value is removed from a hosted prompt before the outbound review and never shown;
// ask: every occurrence in a hosted prompt is raised in the outbound review
// (carry_scan); pass: sent as it is. The shipped default is CONSERVATIVE: every field
// that was not pass before is ask (nothing loosens without the user's answer), and the
// choice is per field, so a later data edit can move one field either way. No shipped
// field is never (the catalogue holds no credential; a never field is a data edit). A
// field with the access-credential flag may not be pass, and a remembered release of
// it is not applied (carry_scan restricts its spans to a live release); no shipped
// field has the flag since decisions.id=956 (the serial is an ordinary ask field). The device's NAME
// is not a fact: its disclosure is the top-level device_name_disclosure.
//
// THE STORED COLUMNS. entity_facts.abstraction_tier2/3 of an owned fact are written
// with a conservative mapping of the disclosure (Disclosure::stored_policy: never ->
// not_permitted, ask -> omit, pass -> pass) and NOTHING READS THEM (owned facts are
// excluded from every PersonalTrack loader; a scan pins that the owned-fact and
// device-seed readers never select them). Rows written before items.id=924 keep their
// stricter not_permitted. No migration.
//
// The configuration/workflow history category of decisions.id=507 is NOT here: it
// is a pointer to the configuration deltas, which live in outputs.db
// (case_store), not a fact.
//
// SUBJECT CLASS (decisions.id=507): an open-vocabulary string whose segments are
// separated by a COLON (device:endpoint:laptop, software:browser), model-assigned
// and user-confirmable, stored as the fact named subject_class. The shape is
// validated here (1 to 4 segments of lowercase letters, digits and underscores,
// each starting with a letter); the vocabulary is open.

use std::sync::OnceLock;

use serde::Deserialize;

/// The fact that carries the subject class.
pub const SUBJECT_CLASS_FIELD: &str = "subject_class";

/// The categories a field may belong to: decisions.id=507's eight, less the
/// configuration history one (a pointer to deltas, not a fact).
pub const CATEGORY_IDS: [&str; 7] = [
    "identity",
    "version_firmware",
    "hardware_profile",
    "network_connectivity",
    "purchase_warranty",
    "access",
    "known_issues",
];

/// A field name containing any of these is refused at validation, whatever its
/// category: the credentials invariant of decisions.id=507.
const CREDENTIAL_WORDS: [&str; 12] = [
    "password",
    "passwd",
    "passcode",
    "passphrase",
    "pin",
    "token",
    "secret",
    "credential",
    "api_key",
    "apikey",
    "private_key",
    "login",
];

const SENSITIVITIES: [&str; 4] = ["general", "personal", "medical", "financial"];

/// items.id=924: what of a device value may reach a hosted step (see the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    Never,
    Ask,
    Pass,
}

impl Disclosure {
    pub fn as_str(self) -> &'static str {
        match self {
            Disclosure::Never => "never",
            Disclosure::Ask => "ask",
            Disclosure::Pass => "pass",
        }
    }

    /// The abstraction policy written to entity_facts for an owned fact (never read).
    pub fn stored_policy(self) -> &'static str {
        match self {
            Disclosure::Never => "not_permitted",
            Disclosure::Ask => "omit",
            Disclosure::Pass => "pass",
        }
    }
}

/// Largest byte cap a field may declare.
pub const FIELD_MAX_BYTES_CEILING: usize = 400;

/// Longest subject class, in bytes, and most segments it may have.
pub const SUBJECT_CLASS_MAX_BYTES: usize = 80;
const SUBJECT_CLASS_MAX_SEGMENTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    pub name: String,
    pub category: String,
    pub label: String,
    pub sensitivity: String,
    /// items.id=924: what may reach a hosted step.
    pub disclosure: Disclosure,
    /// items.id=924: the generalization a review suggests (an ask field only).
    #[serde(default)]
    pub suggestion: Option<String>,
    pub max_bytes: usize,
    /// decisions.id=475: a field that can serve as access-credential metadata (a booking
    /// reference) may not be pass and allows a live release only. No shipped field has it:
    /// the serial number is an ordinary ask field (decisions.id=956, items.id=936).
    #[serde(default)]
    pub access_credential_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub catalogue_version: i64,
    /// items.id=924: the disclosure of a device's NAME (entities.display_name).
    pub device_name_disclosure: Disclosure,
    /// items.id=924: the generalization a review suggests for the name.
    #[serde(default)]
    pub device_name_suggestion: Option<String>,
    pub categories: Vec<Category>,
    pub fields: Vec<FieldDef>,
}

/// Why a value is not accepted. Never carries the value (it is user and model
/// content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("the field is not in the device catalogue")]
    UnknownField,
    #[error("the value is empty")]
    Empty,
    #[error("the value is longer than the field allows")]
    TooLong,
    #[error("the value contains a control character")]
    ControlCharacter,
    #[error("the subject class is not in the form segment:segment (lowercase letters, digits and underscores)")]
    BadSubjectClass,
}

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

fn valid_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl Catalogue {
    pub fn validate(&self) -> Result<(), String> {
        if self.catalogue_version < 1 {
            return Err("catalogue_version must be 1 or more".to_owned());
        }
        if self.categories.is_empty() || self.fields.is_empty() {
            return Err("the catalogue needs categories and fields".to_owned());
        }
        for (i, c) in self.categories.iter().enumerate() {
            if !CATEGORY_IDS.contains(&c.id.as_str()) {
                return Err(format!("category '{}' is not a known category id", c.id));
            }
            if self.categories[..i].iter().any(|o| o.id == c.id) {
                return Err(format!("category '{}' is listed twice", c.id));
            }
            check_label("category", &c.id, &c.label)?;
        }
        for (i, f) in self.fields.iter().enumerate() {
            if !valid_field_name(&f.name) {
                return Err(format!("field name '{}' is not a usable name", f.name));
            }
            if let Some(word) = CREDENTIAL_WORDS.iter().find(|w| f.name.contains(**w)) {
                return Err(format!(
                    "field '{}' names a credential ('{word}'): credentials are never saved",
                    f.name
                ));
            }
            if self.fields[..i].iter().any(|o| o.name == f.name) {
                return Err(format!("field '{}' is listed twice", f.name));
            }
            if !self.categories.iter().any(|c| c.id == f.category) {
                return Err(format!(
                    "field '{}' names category '{}', which is not listed",
                    f.name, f.category
                ));
            }
            check_label("field", &f.name, &f.label)?;
            if !SENSITIVITIES.contains(&f.sensitivity.as_str()) {
                return Err(format!("field '{}': unknown sensitivity", f.name));
            }
            check_suggestion(&f.name, f.disclosure, f.suggestion.as_deref())?;
            if f.max_bytes == 0 || f.max_bytes > FIELD_MAX_BYTES_CEILING {
                return Err(format!(
                    "field '{}': max_bytes must be 1 to {FIELD_MAX_BYTES_CEILING}",
                    f.name
                ));
            }
            // items.id=924: a field that can serve as access-credential metadata is
            // never passed to a hosted step unreviewed.
            if f.access_credential_flag && f.disclosure == Disclosure::Pass {
                return Err(format!(
                    "field '{}' carries the access-credential flag, so it must be ask or never",
                    f.name
                ));
            }
        }
        check_suggestion(
            "device name",
            self.device_name_disclosure,
            self.device_name_suggestion.as_deref(),
        )?;
        if !self.fields.iter().any(|f| f.name == SUBJECT_CLASS_FIELD) {
            return Err(format!("the catalogue must define '{SUBJECT_CLASS_FIELD}'"));
        }
        Ok(())
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The position of a field in the catalogue, for ordering a rendering.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

/// items.id=924: an ask field needs a suggestion (the review's generalization, prompt
/// text, so the label rules apply); a pass or never field has none (never is never
/// shown, pass is never asked).
fn check_suggestion(id: &str, d: Disclosure, suggestion: Option<&str>) -> Result<(), String> {
    match (d, suggestion) {
        (Disclosure::Ask, Some(s)) => check_label("suggestion", id, s),
        (Disclosure::Ask, None) => Err(format!("'{id}' is ask, so it needs a suggestion")),
        (_, Some(_)) => Err(format!("'{id}' is not ask, so it takes no suggestion")),
        (_, None) => Ok(()),
    }
}

/// {token} syntax is refused in a label: render_template() would substitute or
/// strip it (the Prompt Authoring Rule), and a label ends up in a prompt.
fn check_label(kind: &str, id: &str, label: &str) -> Result<(), String> {
    if label.trim().is_empty() || label.len() > 80 {
        return Err(format!("{kind} '{id}': the label must be 1 to 80 bytes"));
    }
    if label.contains('{') || label.contains('}') {
        return Err(format!("{kind} '{id}': a label may not contain braces"));
    }
    if has_control(label) {
        return Err(format!(
            "{kind} '{id}': a label may not contain control characters"
        ));
    }
    Ok(())
}

const BUNDLED: &str = include_str!("../../block_data/device_fields.json");

/// Parse and validate the bundled data once. An Err is a build defect (a test
/// parses the same file): the writers fail closed on it.
pub fn registry() -> Result<&'static Catalogue, String> {
    static REGISTRY: OnceLock<Result<Catalogue, String>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let c: Catalogue = serde_json::from_str(BUNDLED)
                .map_err(|e| format!("block_data/device_fields.json: {e}"))?;
            c.validate()
                .map_err(|e| format!("block_data/device_fields.json: {e}"))?;
            Ok(c)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The shape of a subject class: 1 to 4 colon-separated segments, each starting
/// with a lowercase letter and holding lowercase letters, digits and
/// underscores, at most SUBJECT_CLASS_MAX_BYTES in all.
pub fn valid_subject_class(s: &str) -> bool {
    if s.is_empty() || s.len() > SUBJECT_CLASS_MAX_BYTES {
        return false;
    }
    let segments: Vec<&str> = s.split(':').collect();
    segments.len() <= SUBJECT_CLASS_MAX_SEGMENTS
        && segments.iter().all(|seg| {
            let mut c = seg.chars();
            c.next().is_some_and(|f| f.is_ascii_lowercase())
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// The accepted form of a value for a field: trimmed, non-empty, within the
/// field's byte cap, free of control characters (a newline included: every
/// catalogued value is one line), and for the subject class of the right shape.
pub fn validate_value(def: &FieldDef, raw: &str) -> Result<String, ValueError> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(ValueError::Empty);
    }
    if v.len() > def.max_bytes {
        return Err(ValueError::TooLong);
    }
    if has_control(v) {
        return Err(ValueError::ControlCharacter);
    }
    if def.name == SUBJECT_CLASS_FIELD && !valid_subject_class(v) {
        return Err(ValueError::BadSubjectClass);
    }
    Ok(v.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> Catalogue {
        serde_json::from_str(BUNDLED).expect("the bundled catalogue parses")
    }

    #[test]
    fn the_bundled_catalogue_validates_and_is_pinned() {
        let c = registry().expect("the bundled catalogue must validate");
        assert_eq!(c.catalogue_version, 3);
        // The consent text and the seed list categories that match exactly what
        // is saved, so a new field is a deliberate act: change these with the data.
        let names: Vec<&str> = c.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "subject_class",
                "make",
                "model",
                "os_version",
                "software_version",
                "firmware_version",
                "last_updated",
                "hardware_specs",
                "connection_type",
                "ip_range",
                "isp_context",
                "purchase_date",
                "retailer",
                "serial_number",
                "warranty_expiry",
                "warranty_provider",
                "warranty_contact",
                "access_method",
                "access_location",
                "known_issues",
            ]
        );
        let cats: Vec<&str> = c.categories.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(cats, CATEGORY_IDS);
    }

    #[test]
    fn every_category_is_used_and_no_shipped_field_has_the_credential_flag() {
        let c = registry().unwrap();
        for cat in &c.categories {
            assert!(
                c.fields.iter().any(|f| f.category == cat.id),
                "category '{}' has no field",
                cat.id
            );
        }
        let serial = c.field("serial_number").unwrap();
        // decisions.id=956: the serial is an ordinary ask field, no credential flag.
        assert!(!serial.access_credential_flag);
        assert!(c.fields.iter().all(|f| !f.access_credential_flag));
        assert_eq!(serial.disclosure, Disclosure::Ask);
        assert_eq!(serial.suggestion.as_deref(), Some("[serial number]"));
        assert_eq!(c.device_name_disclosure, Disclosure::Ask);
        assert_eq!(c.device_name_suggestion.as_deref(), Some("[device]"));
    }

    /// items.id=924 D2: the conservative default. Every field that was pass before is
    /// pass, every other one is ask (none is never), each ask field has a suggestion,
    /// and the stored columns get the conservative mapping.
    #[test]
    fn the_shipped_disclosures_are_the_conservative_default() {
        let c = registry().unwrap();
        let ask: Vec<&str> = c
            .fields
            .iter()
            .filter(|f| f.disclosure == Disclosure::Ask)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(
            ask,
            [
                "ip_range",
                "isp_context",
                "purchase_date",
                "retailer",
                "serial_number",
                "warranty_expiry",
                "warranty_provider",
                "warranty_contact",
                "access_method",
                "access_location",
            ]
        );
        assert!(c.fields.iter().all(|f| f.disclosure != Disclosure::Never));
        assert_eq!(
            c.fields
                .iter()
                .filter(|f| f.disclosure == Disclosure::Pass)
                .count(),
            10
        );
        for f in &c.fields {
            assert_eq!(
                f.suggestion.is_some(),
                f.disclosure == Disclosure::Ask,
                "{}",
                f.name
            );
        }
        assert_eq!(Disclosure::Never.stored_policy(), "not_permitted");
        assert_eq!(Disclosure::Ask.stored_policy(), "omit");
        assert_eq!(Disclosure::Pass.stored_policy(), "pass");
    }

    #[test]
    fn no_field_name_can_be_a_credential() {
        for bad in [
            "password",
            "wifi_password",
            "admin_pin",
            "api_key",
            "auth_token",
            "recovery_passphrase",
            "ssh_private_key",
            "login_name",
        ] {
            let mut c = bundled();
            c.fields[1].name = bad.to_owned();
            assert!(c.validate().is_err(), "'{bad}' must be refused");
        }
        // The shipped names are clean. The check is a substring match, so a
        // name that merely contains one of the words is rethought, not allowed.
        for f in &bundled().fields {
            assert!(
                !CREDENTIAL_WORDS.iter().any(|w| f.name.contains(w)),
                "{}",
                f.name
            );
        }
    }

    #[test]
    fn the_validator_refuses_each_kind_of_bad_data() {
        let mut cases: Vec<(&str, Catalogue)> = Vec::new();
        let mut push = |what, edit: &dyn Fn(&mut Catalogue)| {
            let mut c = bundled();
            edit(&mut c);
            cases.push((what, c));
        };
        push("version zero", &|c| c.catalogue_version = 0);
        push("duplicate field", &|c| {
            c.fields[2].name = c.fields[1].name.clone()
        });
        push("unknown category", &|c| {
            c.fields[1].category = "nope".to_owned()
        });
        push("unknown category id listed", &|c| {
            c.categories[0].id = "nope".to_owned()
        });
        push("unknown sensitivity", &|c| {
            c.fields[1].sensitivity = "x".to_owned()
        });
        push("ask without a suggestion", &|c| {
            let i = c.fields.iter().position(|f| f.name == "retailer").unwrap();
            c.fields[i].suggestion = None;
        });
        push("pass with a suggestion", &|c| {
            c.fields[1].suggestion = Some("[make]".to_owned())
        });
        push("never with a suggestion", &|c| {
            c.fields[1].disclosure = Disclosure::Never;
            c.fields[1].suggestion = Some("[make]".to_owned());
        });
        push("brace in a suggestion", &|c| {
            let i = c.fields.iter().position(|f| f.name == "retailer").unwrap();
            c.fields[i].suggestion = Some("{user_input}".to_owned());
        });
        push("device name ask without a suggestion", &|c| {
            c.device_name_suggestion = None
        });
        push("zero cap", &|c| c.fields[1].max_bytes = 0);
        push("cap over the ceiling", &|c| c.fields[1].max_bytes = 401);
        push("brace in a label", &|c| {
            c.fields[1].label = "{user_input}".to_owned()
        });
        push("empty label", &|c| c.fields[1].label = " ".to_owned());
        push("bad name", &|c| c.fields[1].name = "Bad Name".to_owned());
        // decisions.id=956: no shipped field has the flag, so a fixture sets it on a pass
        // field (make). A flagged field may not be pass.
        push("flagged field that is pass", &|c| {
            let i = c.fields.iter().position(|f| f.name == "make").unwrap();
            assert_eq!(c.fields[i].disclosure, Disclosure::Pass);
            c.fields[i].access_credential_flag = true;
        });
        push("no subject_class", &|c| {
            c.fields.retain(|f| f.name != SUBJECT_CLASS_FIELD)
        });
        for (what, c) in cases {
            assert!(c.validate().is_err(), "{what} must be refused");
        }
        assert!(bundled().validate().is_ok());
        // A never field validates, a fixture-flagged field validates as ask or never,
        // and the serial (no flag since decisions.id=956) validates as pass.
        let mut c = bundled();
        let i = c
            .fields
            .iter()
            .position(|f| f.name == "serial_number")
            .unwrap();
        c.fields[i].disclosure = Disclosure::Never;
        c.fields[i].suggestion = None;
        assert!(c.validate().is_ok());
        let mut c = bundled();
        let i = c.fields.iter().position(|f| f.name == "retailer").unwrap();
        c.fields[i].access_credential_flag = true;
        assert!(c.validate().is_ok(), "a flagged ask field is allowed");
        c.fields[i].disclosure = Disclosure::Never;
        c.fields[i].suggestion = None;
        assert!(c.validate().is_ok(), "a flagged never field is allowed");
        let mut c = bundled();
        let i = c
            .fields
            .iter()
            .position(|f| f.name == "serial_number")
            .unwrap();
        c.fields[i].disclosure = Disclosure::Pass;
        c.fields[i].suggestion = None;
        assert!(
            c.validate().is_ok(),
            "the serial is no longer closed upward"
        );
        // An unknown disclosure word does not parse.
        let bad = BUNDLED.replacen("\"disclosure\": \"pass\"", "\"disclosure\": \"maybe\"", 1);
        assert!(serde_json::from_str::<Catalogue>(&bad).is_err());
    }

    #[test]
    fn unknown_keys_in_the_data_file_are_refused() {
        let bad = BUNDLED.replacen(
            "\"name\": \"make\"",
            "\"name\": \"make\", \"surprise\": 1",
            1,
        );
        assert_ne!(bad, BUNDLED);
        assert!(serde_json::from_str::<Catalogue>(&bad).is_err());
    }

    #[test]
    fn subject_class_uses_colons_and_stays_in_shape() {
        for ok in [
            "device",
            "device:endpoint:laptop",
            "software:browser",
            "infrastructure:container:lxc",
            "a:b:c:d",
            "device:wifi6",
        ] {
            assert!(valid_subject_class(ok), "{ok}");
        }
        for bad in [
            "",
            "Device",
            "device.endpoint",
            "device:",
            ":device",
            "device::laptop",
            "a:b:c:d:e",
            "1device",
            "device:end point",
            &"a".repeat(81),
        ] {
            assert!(!valid_subject_class(bad), "{bad}");
        }
    }

    #[test]
    fn a_value_is_trimmed_capped_one_line_and_never_echoed() {
        let c = registry().unwrap();
        let make = c.field("make").unwrap();
        assert_eq!(validate_value(make, "  Netgear  ").unwrap(), "Netgear");
        assert_eq!(validate_value(make, "   "), Err(ValueError::Empty));
        assert_eq!(
            validate_value(make, &"x".repeat(81)),
            Err(ValueError::TooLong)
        );
        assert_eq!(
            validate_value(make, "a\nb"),
            Err(ValueError::ControlCharacter)
        );
        assert_eq!(
            validate_value(make, "a\0b"),
            Err(ValueError::ControlCharacter)
        );
        let class = c.field(SUBJECT_CLASS_FIELD).unwrap();
        assert_eq!(
            validate_value(class, "Device Laptop"),
            Err(ValueError::BadSubjectClass)
        );
        assert_eq!(
            validate_value(class, "device:endpoint:laptop").unwrap(),
            "device:endpoint:laptop"
        );
        // The error text names no value.
        let msg = validate_value(make, "SECRET\nmore")
            .unwrap_err()
            .to_string();
        assert!(!msg.contains("SECRET"));
    }

    #[test]
    fn labels_carry_no_token_syntax_anywhere() {
        let c = registry().unwrap();
        for f in &c.fields {
            assert!(!f.label.contains('{') && !f.label.contains('}'));
        }
        for cat in &c.categories {
            assert!(!cat.label.contains('{') && !cat.label.contains('}'));
        }
    }

    /// items.id=924: the stored abstraction columns of an owned fact are written (the
    /// conservative mapping) and never read: the owned-fact store and the device seed,
    /// the only readers of owned facts that reach a prompt, never name them.
    #[test]
    fn the_stored_abstraction_columns_of_an_owned_fact_are_never_read() {
        for (path, src) in [
            (
                "owned_entity_store.rs",
                include_str!("../persistence/owned_entity_store.rs"),
            ),
            ("device_seeding.rs", include_str!("device_seeding.rs")),
        ] {
            let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap();
            assert!(!prod.contains("abstraction_tier"), "{path}");
        }
    }
}

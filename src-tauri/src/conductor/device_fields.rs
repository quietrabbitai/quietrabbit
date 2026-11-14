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
// sensitivity and the two abstraction policies the fact is written with (the
// existing entity_facts writer defaulted both to pass, which is wrong for a serial
// number or an address), a byte cap, and for the serial number the
// access_credential_flag of decisions.id=475.
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
const POLICIES: [&str; 5] = ["pass", "omit", "summarize", "range_only", "not_permitted"];

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
    pub abstraction_tier2: String,
    pub abstraction_tier3: String,
    pub max_bytes: usize,
    /// decisions.id=475: a serial number can serve as access-credential metadata.
    #[serde(default)]
    pub access_credential_flag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub catalogue_version: i64,
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
            for (which, p) in [("2", &f.abstraction_tier2), ("3", &f.abstraction_tier3)] {
                if !POLICIES.contains(&p.as_str()) {
                    return Err(format!(
                        "field '{}': unknown abstraction policy for tier {which}",
                        f.name
                    ));
                }
            }
            if f.max_bytes == 0 || f.max_bytes > FIELD_MAX_BYTES_CEILING {
                return Err(format!(
                    "field '{}': max_bytes must be 1 to {FIELD_MAX_BYTES_CEILING}",
                    f.name
                ));
            }
            // A field that can identify the device or reach it is never passed
            // upward unreviewed: the flagged serial number and anything marked
            // not_permitted keep both tiers closed.
            if f.access_credential_flag
                && !(f.abstraction_tier2 == "not_permitted"
                    && f.abstraction_tier3 == "not_permitted")
            {
                return Err(format!(
                    "field '{}' carries the access-credential flag, so both tiers must be not_permitted",
                    f.name
                ));
            }
        }
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
        assert_eq!(c.catalogue_version, 1);
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
    fn every_category_is_used_and_the_flagged_field_is_closed_upward() {
        let c = registry().unwrap();
        for cat in &c.categories {
            assert!(
                c.fields.iter().any(|f| f.category == cat.id),
                "category '{}' has no field",
                cat.id
            );
        }
        let serial = c.field("serial_number").unwrap();
        assert!(serial.access_credential_flag);
        assert_eq!(serial.abstraction_tier2, "not_permitted");
        assert_eq!(serial.abstraction_tier3, "not_permitted");
        // Anything that reaches the device is closed upward as well.
        for name in ["access_location", "warranty_contact"] {
            let f = c.field(name).unwrap();
            assert_eq!(f.abstraction_tier2, "not_permitted", "{name}");
            assert_eq!(f.abstraction_tier3, "not_permitted", "{name}");
        }
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
        push("unknown policy", &|c| {
            c.fields[1].abstraction_tier2 = "x".to_owned()
        });
        push("zero cap", &|c| c.fields[1].max_bytes = 0);
        push("cap over the ceiling", &|c| c.fields[1].max_bytes = 401);
        push("brace in a label", &|c| {
            c.fields[1].label = "{user_input}".to_owned()
        });
        push("empty label", &|c| c.fields[1].label = " ".to_owned());
        push("bad name", &|c| c.fields[1].name = "Bad Name".to_owned());
        push("flag without closed tiers", &|c| {
            let i = c
                .fields
                .iter()
                .position(|f| f.name == "serial_number")
                .unwrap();
            c.fields[i].abstraction_tier2 = "pass".to_owned();
        });
        push("no subject_class", &|c| {
            c.fields.retain(|f| f.name != SUBJECT_CLASS_FIELD)
        });
        for (what, c) in cases {
            assert!(c.validate().is_err(), "{what} must be refused");
        }
        assert!(bundled().validate().is_ok());
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
}

// src-tauri/src/conductor/personal_fact_fields.rs
//
// items.id=930 stage 2 (decisions.id=953 rulings 1, 3, 6, 10): the CLOSED CATALOGUE of
// the personal facts that may ever reach an external step, as data
// (block_data/personal_fact_fields.json, the device_fields.rs precedent), the pure
// resolver effective_policy() and the validators. NOT wired into anything: no Gate 1,
// no executor, no store. Stage 3 adds the policy table, items.id=931 the bridge.
//
// PURE: no I/O, no database, no model or provider id (a scan test pins it). The
// entity_facts abstraction_tier2/3 columns are never an input here: they default to
// 'pass' and cannot be told from a choice (decisions.id=953 ruling 2).
//
// RESOLUTION, first match wins (ruling 3):
//   1. not catalogued, or the catalogue unreadable: NotPermitted
//   2. hard floor by sensitivity, never user-overridable: medical and financial are
//      NotPermitted at tiers 2 and 3 (an unknown stored sensitivity counts as
//      financial; the catalogue's own sensitivity is authoritative, so the effective
//      one is the more severe of the two)
//   3. a cross-Persona fact: NotPermitted (v1)
//   4. the user's row for the tier, if present and parseable (pass, summarize, omit,
//      ask). Anything else, including not_permitted (system-only), is Omit. A user
//      pass at tier 3 counts only for a tier3_selectable field, else Omit.
//   5. the catalogue default (Omit at both tiers for every v1 field).
// Ask is reserved (items.id=924): it behaves as Omit and discloses nothing.
//
// User-facing words (placeholder, Chat-BRAND): share = pass, describe = summarize,
// withhold = omit. The store and the resolver speak the policy words.

use std::sync::OnceLock;

use serde::Deserialize;

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

/// Largest byte cap a field may declare.
pub const FIELD_MAX_BYTES_CEILING: usize = 400;

/// Policy words a catalogue default may use.
const DEFAULT_WORDS: [&str; 4] = ["omit", "summarize", "pass", "not_permitted"];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    pub name: String,
    pub label: String,
    pub sensitivity: String,
    pub tier2_default: String,
    pub tier3_default: String,
    /// Only a field marked so may ever be passed at tier 3 (v1: none).
    pub tier3_selectable: bool,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub catalogue_version: i64,
    pub fields: Vec<FieldDef>,
}

/// The external tiers a policy is held for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Two,
    Three,
}

impl Tier {
    /// None for any other number: the caller treats that as Omit.
    pub fn from_number(n: i64) -> Option<Tier> {
        match n {
            2 => Some(Tier::Two),
            3 => Some(Tier::Three),
            _ => None,
        }
    }
}

/// What is done with a catalogued fact at an external tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Pass,
    Summarize,
    Omit,
    /// Reserved (items.id=924): behaves as Omit.
    Ask,
    /// System-only: never a user choice.
    NotPermitted,
}

impl Policy {
    /// The word stored and shown to the validators.
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Pass => "pass",
            Policy::Summarize => "summarize",
            Policy::Omit => "omit",
            Policy::Ask => "ask",
            Policy::NotPermitted => "not_permitted",
        }
    }

    /// A word a USER may store: pass, summarize, omit or the reserved ask.
    pub fn parse_user(s: &str) -> Option<Policy> {
        match s {
            "pass" => Some(Policy::Pass),
            "summarize" => Some(Policy::Summarize),
            "omit" => Some(Policy::Omit),
            "ask" => Some(Policy::Ask),
            _ => None,
        }
    }

    fn parse_default(s: &str) -> Option<Policy> {
        match s {
            "not_permitted" => Some(Policy::NotPermitted),
            other => Policy::parse_user(other),
        }
    }

    /// True only for pass: the one policy that releases the raw value.
    pub fn releases_raw_value(self) -> bool {
        self == Policy::Pass
    }
}

/// Where a fact came from, for the cross-Persona rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    ThisPersona,
    CrossPersona,
}

/// Everything effective_policy reads. The user's row is the RAW string as stored (an
/// unparsable one is Omit), never a parsed value, so garbage cannot be skipped.
#[derive(Debug, Clone, Copy)]
pub struct PolicyInput<'a> {
    pub field_name: &'a str,
    /// The stored fact's sensitivity (entity_facts.sensitivity). Unknown means financial.
    pub stored_sensitivity: &'a str,
    pub origin: Origin,
    pub tier: Tier,
    /// The user's fact_disclosure_policy value for this field and tier, if a row exists.
    pub user_policy: Option<&'a str>,
}

fn severity(s: &str) -> u8 {
    match s {
        "general" => 1,
        "personal" => 2,
        "medical" => 3,
        _ => 4, // financial, and anything unknown
    }
}

/// The effective sensitivity: the more severe of the stored and the catalogue's.
pub fn effective_sensitivity<'a>(def: &'a FieldDef, stored: &'a str) -> &'a str {
    if severity(stored) > severity(&def.sensitivity) {
        match severity(stored) {
            1 => "general",
            2 => "personal",
            3 => "medical",
            _ => "financial",
        }
    } else {
        &def.sensitivity
    }
}

/// The pure resolver. `catalogue` is the registry() result: an unreadable catalogue
/// is NotPermitted for everything (fail closed).
pub fn effective_policy(catalogue: Result<&Catalogue, ()>, input: &PolicyInput<'_>) -> Policy {
    let Ok(catalogue) = catalogue else {
        return Policy::NotPermitted;
    };
    let Some(def) = catalogue.field(input.field_name) else {
        return Policy::NotPermitted;
    };
    if severity(effective_sensitivity(def, input.stored_sensitivity)) >= 3 {
        return Policy::NotPermitted;
    }
    if input.origin == Origin::CrossPersona {
        return Policy::NotPermitted;
    }
    if let Some(raw) = input.user_policy {
        return match Policy::parse_user(raw) {
            Some(Policy::Pass) if input.tier == Tier::Three && !def.tier3_selectable => {
                Policy::Omit
            }
            Some(p) => p,
            None => Policy::Omit,
        };
    }
    let default = match input.tier {
        Tier::Two => &def.tier2_default,
        Tier::Three => &def.tier3_default,
    };
    match Policy::parse_default(default) {
        // A default pass is validated away for tier 3 unless selectable, but never
        // trust that here: the resolver re-applies the rule.
        Some(Policy::Pass) if input.tier == Tier::Three && !def.tier3_selectable => Policy::Omit,
        Some(p) => p,
        None => Policy::Omit,
    }
}

/// Why a USER choice is refused (never carries a value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChoiceError {
    #[error("the field is not in the personal fact catalogue")]
    UnknownField,
    #[error("the policy is not one a user may choose")]
    UnknownPolicy,
    #[error("this field may not be shared: its sensitivity is closed to external steps")]
    FloorForbidsPass,
    #[error("this field may not be shared at tier 3")]
    Tier3NotSelectable,
}

/// Validate a user's choice for (field, tier) against the catalogue and the hard
/// floors (decisions.id=953 ruling 6). Only a PASS can widen disclosure, so only a
/// pass is checked against the floors: omit, summarize and ask are always allowed
/// for a catalogued name. The caller (the set_fact_policy writer) uses this.
pub fn check_user_choice(
    catalogue: &Catalogue,
    field_name: &str,
    tier: Tier,
    policy: &str,
) -> Result<Policy, ChoiceError> {
    let def = catalogue
        .field(field_name)
        .ok_or(ChoiceError::UnknownField)?;
    let p = Policy::parse_user(policy).ok_or(ChoiceError::UnknownPolicy)?;
    if p == Policy::Pass {
        if severity(&def.sensitivity) >= 3 {
            return Err(ChoiceError::FloorForbidsPass);
        }
        if tier == Tier::Three && !def.tier3_selectable {
            return Err(ChoiceError::Tier3NotSelectable);
        }
    }
    Ok(p)
}

/// Why a value is not accepted. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValueError {
    #[error("the value is empty")]
    Empty,
    #[error("the value is longer than the field allows")]
    TooLong,
    #[error("the value contains a control character")]
    ControlCharacter,
}

/// The accepted form of a value for a field: trimmed, non-empty, within the byte cap,
/// one line. A value that fails is WITHHELD by the bridge (ruling 10).
pub fn validate_value(def: &FieldDef, raw: &str) -> Result<String, ValueError> {
    let v = raw.trim();
    if v.is_empty() {
        return Err(ValueError::Empty);
    }
    if v.len() > def.max_bytes {
        return Err(ValueError::TooLong);
    }
    if v.chars().any(|c| c.is_control()) {
        return Err(ValueError::ControlCharacter);
    }
    Ok(v.to_owned())
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
        if self.fields.is_empty() {
            return Err("the catalogue needs fields".to_owned());
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
            if f.label.trim().is_empty()
                || f.label.len() > 80
                || f.label.contains('{')
                || f.label.contains('}')
                || f.label.chars().any(|c| c.is_control())
            {
                return Err(format!(
                    "field '{}': the label must be 1 to 80 bytes with no braces or control characters",
                    f.name
                ));
            }
            if !matches!(
                f.sensitivity.as_str(),
                "general" | "personal" | "medical" | "financial"
            ) {
                return Err(format!("field '{}': unknown sensitivity", f.name));
            }
            if f.max_bytes == 0 || f.max_bytes > FIELD_MAX_BYTES_CEILING {
                return Err(format!(
                    "field '{}': max_bytes must be 1 to {FIELD_MAX_BYTES_CEILING}",
                    f.name
                ));
            }
            for (which, d) in [("2", &f.tier2_default), ("3", &f.tier3_default)] {
                if !DEFAULT_WORDS.contains(&d.as_str()) {
                    return Err(format!(
                        "field '{}': unknown default policy for tier {which}",
                        f.name
                    ));
                }
            }
            // A medical or financial field is closed to both external tiers and can
            // never be selectable: the floor is data-independent, so refuse data that
            // pretends otherwise.
            if severity(&f.sensitivity) >= 3
                && (f.tier2_default != "not_permitted"
                    || f.tier3_default != "not_permitted"
                    || f.tier3_selectable)
            {
                return Err(format!(
                    "field '{}' is {}: both defaults must be not_permitted and tier 3 not selectable",
                    f.name, f.sensitivity
                ));
            }
            // Pass is never a default at tier 3 unless the field is selectable there.
            if f.tier3_default == "pass" && !f.tier3_selectable {
                return Err(format!(
                    "field '{}': a tier 3 default of pass needs tier3_selectable",
                    f.name
                ));
            }
        }
        Ok(())
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }
}

const BUNDLED: &str = include_str!("../../block_data/personal_fact_fields.json");

/// Parse and validate the bundled data once. An Err is a build defect (a test parses
/// the same file); every consumer fails closed on it.
pub fn registry() -> Result<&'static Catalogue, String> {
    static REGISTRY: OnceLock<Result<Catalogue, String>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let c: Catalogue = serde_json::from_str(BUNDLED)
                .map_err(|e| format!("block_data/personal_fact_fields.json: {e}"))?;
            c.validate()
                .map_err(|e| format!("block_data/personal_fact_fields.json: {e}"))?;
            Ok(c)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled() -> Catalogue {
        serde_json::from_str(BUNDLED).expect("the bundled catalogue parses")
    }

    fn input<'a>(
        name: &'a str,
        stored: &'a str,
        tier: Tier,
        user: Option<&'a str>,
    ) -> PolicyInput<'a> {
        PolicyInput {
            field_name: name,
            stored_sensitivity: stored,
            origin: Origin::ThisPersona,
            tier,
            user_policy: user,
        }
    }

    fn resolve(c: &Catalogue, i: &PolicyInput<'_>) -> Policy {
        effective_policy(Ok(c), i)
    }

    #[test]
    fn the_bundled_catalogue_validates_and_is_pinned_to_four_names() {
        let c = registry().expect("the bundled catalogue must validate");
        assert_eq!(c.catalogue_version, 1);
        let names: Vec<&str> = c.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["location_city", "job_title", "employer", "industry"]
        );
        for f in &c.fields {
            assert_eq!(f.tier2_default, "omit", "{}", f.name);
            assert_eq!(f.tier3_default, "omit", "{}", f.name);
            assert!(!f.tier3_selectable, "{}", f.name);
            // classify_sensitivity returns personal for all four names.
            assert_eq!(f.sensitivity, "personal", "{}", f.name);
        }
    }

    #[test]
    fn no_policy_means_omit_at_both_tiers_for_every_v1_field() {
        let c = bundled();
        for f in &c.fields {
            for tier in [Tier::Two, Tier::Three] {
                assert_eq!(
                    resolve(&c, &input(&f.name, &f.sensitivity, tier, None)),
                    Policy::Omit,
                    "{}",
                    f.name
                );
            }
        }
    }

    #[test]
    fn the_user_choice_table_sensitivity_by_tier_by_policy() {
        let c = bundled();
        for stored in ["general", "personal", "medical", "financial", "weird", ""] {
            for tier in [Tier::Two, Tier::Three] {
                for user in [
                    None,
                    Some("pass"),
                    Some("summarize"),
                    Some("omit"),
                    Some("ask"),
                    Some("garbage"),
                    Some("not_permitted"),
                    Some(""),
                ] {
                    let got = resolve(&c, &input("job_title", stored, tier, user));
                    let want = if matches!(stored, "medical" | "financial" | "weird" | "") {
                        Policy::NotPermitted // the floor beats any user row; unknown counts as financial
                    } else {
                        match (user, tier) {
                            (Some("pass"), Tier::Two) => Policy::Pass,
                            (Some("pass"), Tier::Three) => Policy::Omit, // not tier3_selectable
                            (Some("summarize"), _) => Policy::Summarize,
                            (Some("ask"), _) => Policy::Ask,
                            _ => Policy::Omit, // none, omit, garbage, not_permitted, empty
                        }
                    };
                    assert_eq!(got, want, "stored={stored} tier={tier:?} user={user:?}");
                    // Only an explicit pass at tier 2 releases a raw value.
                    assert_eq!(
                        got.releases_raw_value(),
                        user == Some("pass")
                            && tier == Tier::Two
                            && matches!(stored, "general" | "personal")
                    );
                }
            }
        }
    }

    #[test]
    fn the_catalogue_sensitivity_is_authoritative_as_the_more_severe() {
        let mut c = bundled();
        // Stored general, catalogue personal: personal. Stored financial beats both.
        let def = c.field("location_city").unwrap();
        assert_eq!(effective_sensitivity(def, "general"), "personal");
        assert_eq!(effective_sensitivity(def, "financial"), "financial");
        // A catalogue that marks a field medical closes it even if the stored fact is general.
        c.fields[1].sensitivity = "medical".to_owned();
        assert_eq!(
            resolve(&c, &input("job_title", "general", Tier::Two, Some("pass"))),
            Policy::NotPermitted
        );
    }

    #[test]
    fn a_cross_persona_fact_and_an_unknown_name_and_an_unreadable_catalogue_are_not_permitted() {
        let c = bundled();
        let mut i = input("job_title", "general", Tier::Two, Some("pass"));
        i.origin = Origin::CrossPersona;
        assert_eq!(resolve(&c, &i), Policy::NotPermitted);
        assert_eq!(
            resolve(
                &c,
                &input("sender_name", "general", Tier::Two, Some("pass"))
            ),
            Policy::NotPermitted
        );
        assert_eq!(
            effective_policy(
                Err(()),
                &input("job_title", "general", Tier::Two, Some("pass"))
            ),
            Policy::NotPermitted
        );
    }

    #[test]
    fn tier3_pass_needs_a_selectable_field_for_a_row_and_for_a_default() {
        let mut c = bundled();
        c.fields[1].tier3_selectable = true;
        assert_eq!(
            resolve(
                &c,
                &input("job_title", "general", Tier::Three, Some("pass"))
            ),
            Policy::Pass
        );
        assert_eq!(
            resolve(
                &c,
                &input("employer", "personal", Tier::Three, Some("pass"))
            ),
            Policy::Omit
        );
        // A default of pass on a non-selectable field is refused by validate and, if it
        // got in anyway, never honoured by the resolver.
        let mut d = bundled();
        d.fields[2].tier3_default = "pass".to_owned();
        assert!(d.validate().is_err());
        assert_eq!(
            resolve(&d, &input("employer", "personal", Tier::Three, None)),
            Policy::Omit
        );
        // Tier numbers other than 2 and 3 do not exist.
        assert!(Tier::from_number(1).is_none() && Tier::from_number(4).is_none());
        assert_eq!(Tier::from_number(3), Some(Tier::Three));
    }

    #[test]
    fn check_user_choice_applies_the_catalogue_and_the_floors() {
        let c = bundled();
        assert_eq!(
            check_user_choice(&c, "job_title", Tier::Two, "pass"),
            Ok(Policy::Pass)
        );
        assert_eq!(
            check_user_choice(&c, "job_title", Tier::Two, "ask"),
            Ok(Policy::Ask)
        );
        assert_eq!(
            check_user_choice(&c, "job_title", Tier::Three, "pass"),
            Err(ChoiceError::Tier3NotSelectable)
        );
        assert_eq!(
            check_user_choice(&c, "job_title", Tier::Three, "omit"),
            Ok(Policy::Omit)
        );
        assert_eq!(
            check_user_choice(&c, "income_range", Tier::Two, "omit"),
            Err(ChoiceError::UnknownField)
        );
        for bad in ["not_permitted", "share", "", "PASS"] {
            assert_eq!(
                check_user_choice(&c, "job_title", Tier::Two, bad),
                Err(ChoiceError::UnknownPolicy),
                "{bad}"
            );
        }
        // A medical or financial catalogue field refuses pass but still takes omit.
        let mut m = bundled();
        m.fields[1].sensitivity = "financial".to_owned();
        assert_eq!(
            check_user_choice(&m, "job_title", Tier::Two, "pass"),
            Err(ChoiceError::FloorForbidsPass)
        );
        assert_eq!(
            check_user_choice(&m, "job_title", Tier::Two, "omit"),
            Ok(Policy::Omit)
        );
    }

    #[test]
    fn validate_refuses_bad_data() {
        type Mutation = fn(&mut Catalogue);
        let cases: [(&str, Mutation); 9] = [
            ("credential name", |c| {
                c.fields[0].name = "login_city".to_owned()
            }),
            ("duplicate", |c| {
                c.fields[1].name = "location_city".to_owned()
            }),
            ("bad name", |c| c.fields[0].name = "City".to_owned()),
            ("brace label", |c| c.fields[0].label = "{city}".to_owned()),
            ("bad sensitivity", |c| {
                c.fields[0].sensitivity = "secret".to_owned()
            }),
            ("zero bytes", |c| c.fields[0].max_bytes = 0),
            ("too many bytes", |c| {
                c.fields[0].max_bytes = FIELD_MAX_BYTES_CEILING + 1
            }),
            ("bad default", |c| {
                c.fields[0].tier2_default = "general".to_owned()
            }),
            ("medical not closed", |c| {
                c.fields[0].sensitivity = "medical".to_owned()
            }),
        ];
        for (what, mutate) in cases {
            let mut c = bundled();
            mutate(&mut c);
            assert!(c.validate().is_err(), "{what}");
        }
        let mut empty = bundled();
        empty.fields.clear();
        assert!(empty.validate().is_err());
        let mut v0 = bundled();
        v0.catalogue_version = 0;
        assert!(v0.validate().is_err());
    }

    #[test]
    fn unknown_json_keys_are_refused() {
        let with_alias = BUNDLED.replacen("\"label\"", "\"alias\": \"x\", \"label\"", 1);
        assert!(serde_json::from_str::<Catalogue>(&with_alias).is_err());
    }

    #[test]
    fn validate_value_trims_and_refuses() {
        let c = bundled();
        let def = c.field("location_city").unwrap();
        assert_eq!(
            validate_value(def, "  Portland ").as_deref(),
            Ok("Portland")
        );
        assert_eq!(validate_value(def, "   "), Err(ValueError::Empty));
        assert_eq!(
            validate_value(def, &"x".repeat(81)),
            Err(ValueError::TooLong)
        );
        assert_eq!(
            validate_value(def, "a\nb"),
            Err(ValueError::ControlCharacter)
        );
    }

    #[test]
    fn the_module_is_pure_and_names_no_column_provider_or_model() {
        let src = include_str!("personal_fact_fields.rs");
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "sqlx",
            "std::fs",
            "tokio",
            "reqwest",
            "abstraction_tier",
            "provider_id",
            "ollama",
            "llama",
            "qwen",
        ] {
            assert!(!code.contains(banned), "{banned}");
        }
        // Wired into nothing yet: only the module declaration and this file name it.
        assert!(registry().is_ok());
    }
}

// src-tauri/src/conductor/personal_fact_bridge.rs
//
// items.id=931, decisions.id=953 ruling 4: the ONE constructor of bridged Gate 1 fields.
//
// A catalogued, singleton, same-Persona entity fact becomes a PersonalField on
// PersonalTrack.fields (the single source for Gate 1, Gate 2, the voice scan and step
// sensitivity). Its abstraction_tier2 and abstraction_tier3 carry the EFFECTIVE policy
// word (personal_fact_fields::effective_policy) at INITIALIZE, so Gate 1's existing
// apply_abstraction decides what is sent. The bridge NEVER reads the entity_facts
// abstraction columns (they default to 'pass' and cannot tell a default from a choice):
// EntityFact does not carry them and the loader does not select them (a scan pins it).
//
// PURE: no I/O. The caller (lifecycle::build_personal_track) supplies the facts that
// already passed apply_entity_fact_provenance_check, the catalogue and the user's
// policies. A bridged field is recognised later by BRIDGE_SOURCE_ID, which nothing else
// writes, so the mapping to the gate track and the re-read can tell it apart.

use std::collections::HashMap;

use crate::conductor::personal_fact_fields::{
    effective_policy, effective_sensitivity, validate_value, Catalogue, Origin, Policy,
    PolicyInput, Tier,
};
use crate::conductor::types::{EntityFact, PersonalField};
use crate::persistence::fact_policy_store::UserPolicies;

/// The source_id of every bridged field and of nothing else.
pub const BRIDGE_SOURCE_ID: &str = "entity_fact_bridge";

/// True for a field this module built.
pub fn is_bridged(field: &PersonalField) -> bool {
    is_bridged_source(&field.source_id)
}

/// The same test on a source id, for a reader holding the gate track's field (the carry
/// scan, items.id=933), so the marker stays named in this file only.
pub fn is_bridged_source(source_id: &str) -> bool {
    source_id == BRIDGE_SOURCE_ID
}

/// How strict a policy word is. Higher is stricter. An unknown word is as strict as
/// omit (fail closed). items.id=924: ask sends the raw value only on the user's answer
/// in the review, summarize sends its label unreviewed, so ask ranks between pass and
/// summarize.
fn strictness(word: &str) -> u8 {
    match word {
        "pass" => 0,
        "ask" => 1,
        "summarize" => 2,
        "not_permitted" => 4,
        // omit and anything unknown
        _ => 3,
    }
}

/// The STRICTER of two policy words (decisions.id=953 ruling 7): not_permitted over
/// omit (unknown counts as omit) over summarize over ask over pass. Equal strictness
/// keeps `a`, so pass against pass stays pass (an explicit pass survives a re-read
/// that still says pass, and keeps no floor clamp). Returns a word Gate 1 understands,
/// or "ask", which executor::to_gate_track_asks resolves for the step.
pub fn stricter(a: &str, b: &str) -> &'static str {
    let s = strictness(a).max(strictness(b));
    match s {
        0 => "pass",
        1 => "ask",
        2 => "summarize",
        4 => "not_permitted",
        _ => "omit",
    }
}

/// The word stored on the field for a resolved policy.
fn policy_word(p: Policy) -> &'static str {
    match p {
        Policy::Pass => "pass",
        Policy::Summarize => "summarize",
        Policy::Omit => "omit",
        Policy::Ask => "ask",
        Policy::NotPermitted => "not_permitted",
    }
}

fn severity_of(sensitivity: &str) -> i32 {
    match sensitivity {
        "general" => 1,
        "personal" => 2,
        // medical, financial and anything unknown
        _ => 3,
    }
}

/// `{` and `}` become `(` and `)` in a bridged value, the seeding::clean convention
/// (decisions.id=953 amendment, items.id=931). KEPT AS DEFENCE IN DEPTH since
/// items.id=932 made render_template single-pass (before it, a value carrying
/// `{user_input}` could be expanded by a later substitution). The cleaned text is also
/// what Gate 2, the voice scan and step sensitivity read. The executor's renders apply
/// it again to every projected value (executor::insert_projected_fields, items.id=933).
/// It changes nothing about what the bridge releases.
pub(crate) fn clean_braces(v: &str) -> String {
    v.replace('{', "(").replace('}', ")")
}

/// The effective policy word for a bridged field at `tier`, against `policies`.
/// Used at INITIALIZE (bridge) and again by the executor's re-read.
pub fn resolve_word(
    catalogue: Result<&Catalogue, ()>,
    field_name: &str,
    stored_sensitivity: &str,
    origin: Origin,
    tier: Tier,
    policies: &UserPolicies,
) -> &'static str {
    policy_word(effective_policy(
        catalogue,
        &PolicyInput {
            field_name,
            stored_sensitivity,
            origin,
            tier,
            user_policy: policies.lookup(field_name, tier),
        },
    ))
}

/// Build the bridged fields from facts that already passed the provenance check.
///
/// Qualifies: entity_id None (a singleton), catalogued name. Everything else is left
/// alone (entity-scoped facts, uncatalogued names). Origin is ThisPersona only when
/// source_persona_id equals `persona_id`; any other is CrossPersona and resolves to
/// not_permitted (v1). A value that fails the field validator is kept at not_permitted
/// so Gate 1 lists it as withheld and never releases it. A name that appears twice
/// (it cannot in a healthy store: the partial unique index) is dropped entirely.
/// `catalogue` Err (unreadable) bridges nothing.
pub fn bridge_fields<'a>(
    facts: impl Iterator<Item = &'a EntityFact>,
    persona_id: &str,
    catalogue: Result<&Catalogue, ()>,
    policies: &UserPolicies,
) -> Vec<PersonalField> {
    let Ok(cat) = catalogue else {
        log::warn!("personal fact catalogue unreadable: no fact is bridged");
        return Vec::new();
    };
    let mut built: Vec<PersonalField> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut dropped: Vec<String> = Vec::new();
    for fact in facts {
        if fact.entity_id.is_some() {
            continue;
        }
        let Some(def) = cat.field(&fact.field_name) else {
            continue;
        };
        if seen.contains_key(&fact.field_name) {
            dropped.push(fact.field_name.clone());
            continue;
        }
        let origin = if fact.source_persona_id == persona_id {
            Origin::ThisPersona
        } else {
            Origin::CrossPersona
        };
        let sensitivity = effective_sensitivity(def, &fact.sensitivity).to_owned();
        let (value, valid) = match validate_value(def, &fact.field_value) {
            Ok(v) => (clean_braces(&v), true),
            Err(_) => (clean_braces(&fact.field_value), false),
        };
        let word = |tier: Tier| -> &'static str {
            if !valid {
                return "not_permitted";
            }
            resolve_word(
                Ok(cat),
                &fact.field_name,
                &sensitivity,
                origin,
                tier,
                policies,
            )
        };
        let tier2 = word(Tier::Two).to_owned();
        let tier3 = word(Tier::Three).to_owned();
        seen.insert(fact.field_name.clone(), built.len());
        built.push(PersonalField {
            field_name: fact.field_name.clone(),
            field_value: value,
            sensitivity_severity: severity_of(&sensitivity).max(fact.sensitivity_severity.min(3)),
            sensitivity,
            source_id: BRIDGE_SOURCE_ID.to_owned(),
            abstraction_tier2: tier2,
            abstraction_tier3: tier3,
        });
    }
    for name in dropped {
        // A duplicate is dropped whole, the first copy included (fail closed).
        if let Some(&i) = seen.get(&name) {
            built[i].field_name.clear();
        }
        log::warn!("personal fact '{name}' appears twice: not bridged");
    }
    built.retain(|f| !f.field_name.is_empty());
    built
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::personal_fact_fields::registry;

    fn fact(name: &str, value: &str, entity: Option<&str>, source: &str) -> EntityFact {
        EntityFact {
            id: format!("id-{name}"),
            entity_id: entity.map(str::to_owned),
            field_name: name.to_owned(),
            field_value: value.to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_persona_id: source.to_owned(),
            cross_persona_export: false,
            origin_persona_id: None,
        }
    }

    fn none() -> UserPolicies {
        UserPolicies::Rows(Vec::new())
    }

    #[test]
    fn stricter_orders_pass_summarize_omit_not_permitted() {
        assert_eq!(stricter("pass", "summarize"), "summarize");
        assert_eq!(stricter("summarize", "omit"), "omit");
        assert_eq!(stricter("omit", "pass"), "omit");
        assert_eq!(stricter("not_permitted", "pass"), "not_permitted");
        assert_eq!(stricter("pass", "pass"), "pass");
        // items.id=924: ask is between pass and summarize; garbage counts as omit
        assert_eq!(stricter("pass", "ask"), "ask");
        assert_eq!(stricter("ask", "pass"), "ask");
        assert_eq!(stricter("ask", "summarize"), "summarize");
        assert_eq!(stricter("omit", "ask"), "omit");
        assert_eq!(stricter("ask", "ask"), "ask");
        assert_eq!(stricter("pass", "???"), "omit");
        assert_eq!(stricter("summarize", "???"), "omit");
    }

    #[test]
    fn a_singleton_catalogued_fact_bridges_at_omit_by_default() {
        let facts = [fact("job_title", "Engineer", None, "p1")];
        let out = bridge_fields(facts.iter(), "p1", registry().map_err(|_| ()), &none());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source_id, BRIDGE_SOURCE_ID);
        assert_eq!(out[0].abstraction_tier2, "omit");
        assert_eq!(out[0].abstraction_tier3, "omit");
    }

    #[test]
    fn entity_scoped_uncatalogued_and_unreadable_catalogue_bridge_nothing() {
        let facts = [
            fact("job_title", "x", Some("e1"), "p1"),
            fact("favourite_colour", "x", None, "p1"),
        ];
        assert!(bridge_fields(facts.iter(), "p1", registry().map_err(|_| ()), &none()).is_empty());
        let one = [fact("job_title", "x", None, "p1")];
        assert!(bridge_fields(one.iter(), "p1", Err(()), &none()).is_empty());
    }

    #[test]
    fn a_cross_persona_fact_is_not_permitted_and_a_bad_value_is_withheld() {
        let facts = [
            fact("employer", "Acme", None, "other"),
            fact("job_title", &"x".repeat(500), None, "p1"),
        ];
        let out = bridge_fields(facts.iter(), "p1", registry().map_err(|_| ()), &none());
        assert_eq!(out.len(), 2);
        for f in &out {
            assert_eq!(f.abstraction_tier2, "not_permitted", "{}", f.field_name);
            assert_eq!(f.abstraction_tier3, "not_permitted");
        }
    }

    #[test]
    fn braces_in_a_value_become_parentheses() {
        let facts = [fact(
            "job_title",
            "A {user_input} and {previous_output}",
            None,
            "p1",
        )];
        let out = bridge_fields(facts.iter(), "p1", registry().map_err(|_| ()), &none());
        assert_eq!(out[0].field_value, "A (user_input) and (previous_output)");
        assert!(!out[0].field_value.contains(['{', '}']));
    }

    #[test]
    fn a_duplicate_name_is_dropped_whole() {
        let facts = [
            fact("industry", "A", None, "p1"),
            fact("industry", "B", None, "p1"),
        ];
        assert!(bridge_fields(facts.iter(), "p1", registry().map_err(|_| ()), &none()).is_empty());
    }

    // -- T6: source scans (items.id=931, decisions.id=953 ruling 12) ------------------

    /// Production code of a source file: cut at the test module, drop comment lines.
    fn code_of(src: &str) -> String {
        let prod = src.split("#[cfg(test)]\nmod ").next().unwrap();
        prod.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn source_files() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.to_string_lossy().into_owned();
                    out.push((name, std::fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out
    }

    /// ONE constructor of bridged fields: the marker is named nowhere else in
    /// production code, and the only non-test callers of add_field are the bridge's one
    /// call site in lifecycle.rs and the privacy gate-track conversion in executor.rs
    /// (a different type, privacy::types::PersonalTrack).
    #[test]
    fn t6_one_constructor_and_one_add_field_caller() {
        let mut add_field_callers: Vec<(String, usize)> = Vec::new();
        for (name, src) in source_files() {
            let code = code_of(&src);
            if !name.ends_with("personal_fact_bridge.rs") {
                assert!(
                    !code.contains("BRIDGE_SOURCE_ID"),
                    "{name} names the bridge marker"
                );
            }
            let n = code.matches(".add_field(").count();
            if n > 0 {
                add_field_callers.push((name, n));
            }
        }
        add_field_callers.sort();
        let tails: Vec<(&str, usize)> = add_field_callers
            .iter()
            .map(|(n, c)| (n.rsplit("src/").next().unwrap(), *c))
            .collect();
        assert_eq!(
            tails,
            [("conductor/executor.rs", 1), ("conductor/lifecycle.rs", 1)],
            "{tails:?}"
        );
        let lc = code_of(include_str!("lifecycle.rs"));
        let site = lc.split(".add_field(").next().unwrap();
        assert!(
            site.rsplit("async fn ")
                .next()
                .unwrap()
                .starts_with("bridge_personal_facts("),
            "the one add_field call in lifecycle.rs is in bridge_personal_facts"
        );
        let ex = code_of(include_str!("executor.rs"));
        assert!(
            ex.contains("gt.add_field("),
            "the executor call is the gate track's"
        );
    }

    /// The bridge never reads the entity_facts abstraction columns: EntityFact does not
    /// carry them, the loader does not select them and the bridge names neither them nor
    /// any SQL.
    #[test]
    fn t6_the_bridge_never_reads_the_entity_facts_abstraction_columns() {
        let types = include_str!("types.rs");
        let body = types
            .split("pub struct EntityFact {")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("EntityFact is in types.rs");
        assert!(!body.contains("abstraction"), "{body}");
        let store = include_str!("../persistence/personal_store.rs");
        let loader = store
            .split("pub async fn load_entity_facts_for_context(")
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .expect("the loader is in personal_store.rs");
        assert!(!loader.contains("abstraction"), "{loader}");
        let bridge = code_of(include_str!("personal_fact_bridge.rs"));
        for banned in [
            "sqlx",
            "SELECT",
            "entity_facts",
            "fact.abstraction",
            "FROM ",
        ] {
            assert!(!bridge.contains(banned), "the bridge names {banned}");
        }
    }
}

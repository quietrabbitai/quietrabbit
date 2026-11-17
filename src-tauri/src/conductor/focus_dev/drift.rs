// src-tauri/src/conductor/focus_dev/drift.rs
//
// Drift check (decisions.id=951 amendment 3): every checked-in Focus spec has an emitted
// migration or baseline fragment whose header names the same Focus and carries the
// SHA-256 of the spec's current text. Pure: the caller supplies file names and texts.
// Vacuous until the first spec is checked in (focus_specs/ does not exist yet).

use crate::conductor::focus_dev::render::spec_sha256;
use crate::conductor::focus_dev::spec::FocusSpec;

/// Reads the header lines "-- focus_dev: <focus_id> spec-sha256 <hex>" of one SQL text.
fn headers(sql: &str) -> Vec<(String, String)> {
    sql.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("-- focus_dev: ")?;
            let (id, sha) = rest.split_once(" spec-sha256 ")?;
            Some((id.trim().to_owned(), sha.trim().to_owned()))
        })
        .collect()
}

/// One problem per spec that has no emitted file matching its current hash.
/// `specs` and `sql_files` are (name, text) pairs.
pub fn check(specs: &[(String, String)], sql_files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, text) in specs {
        let focus_id = match FocusSpec::from_json(text) {
            Ok(s) => s.focus.focus_id,
            Err(e) => {
                problems.push(format!("{name}: does not parse: {e}"));
                continue;
            }
        };
        let sha = spec_sha256(text);
        let matched = sql_files.iter().any(|(_, sql)| {
            headers(sql)
                .iter()
                .any(|(id, h)| id == &focus_id && h == &sha)
        });
        if !matched {
            problems.push(format!(
                "{name}: no emitted migration or baseline fragment carries spec-sha256 {sha} for Focus '{focus_id}', run focus_dev emit-migration"
            ));
        }
    }
    problems
}

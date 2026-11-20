// src-tauri/src/conductor/resume_staleness.rs
//
// items.id=681 stage 3 (decisions.id=865): the resume staleness check.
//
// A snapshot's personal_context_manifest holds, for each field of the run's
// PersonalTrack at checkpoint time, a hash of name:value. On resume
// rehydrate_focus_run rebuilds a FRESH PersonalTrack and calls `check`. The run
// is STALE if a field a COMPLETED step used has changed or is gone, because that
// step's output (in the snapshot's task track) was written under the old value
// and can flow on through {previous_output} or an output variable.
//
// WHAT IT COVERS: the manifest's field_hashes, which are PersonalTrack.fields,
// which are the catalogued singleton facts bridged by items.id=931 (location_city,
// job_title, employer, industry). Nothing else.
// WHAT IT DOES NOT COVER (documented limits, decisions.id=865 / items.id=681):
//   - entity (device), group and persona_context facts;
//   - the disclosure POLICY: the manifest holds no policy, so a policy tightened
//     while a run is parked is not detected for output already completed (the
//     executor re-reads the policy before each later step, decisions.id=953);
//   - source_versions: guide and operator versions are in the manifest but are
//     NOT compared here (a .focus edit or an app update is not the user's data
//     changing). PersonalContextManifest::matches() compares them and has no
//     caller outside its own tests; it is left alone.
//   - a confirmed cross-Persona fact released raw to a local step: both resume
//     paths pass an empty confirmed-id set, so the fact is absent on resume, reads
//     as deleted, and a resume of a run whose completed steps used it is cancelled.
//
// "USED" means a field in the disclosure buffer of a COMPLETED step (a step in
// the restored task track), whether or not the step's current definition still
// declares it: the buffer is the record of what was approved for that step. The
// step that is about to be re-run is excluded (its buffer is rewritten by
// execute_once before it is read).
//
// HANDLING (A1): the run is cancelled with PERSONAL_CHANGED_WHILE_WAITING, by the
// same guarded transaction the expiry sweep uses. This module is pure except
// `cancel_stale_resume`, the one shared helper the three resume paths call.

use std::collections::{BTreeSet, HashMap};

use crate::conductor::types::{PersonalTrack, SharedStateTrack, TaskTrack};

/// The plain message a run carries when it is cancelled for a stale resume.
pub const PERSONAL_CHANGED_WHILE_WAITING: &str = "personal information changed while waiting";

/// Why a resume is stale (for the log; the user sees one message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// A field a completed step used changed or is gone.
    FieldsChanged { count: usize },
    /// The snapshot has a manifest that cannot be read (fails closed).
    ManifestUnreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    Stale(StaleReason),
}

#[derive(serde::Deserialize)]
struct ManifestFields {
    #[serde(default)]
    field_hashes: HashMap<String, String>,
}

/// Compare a snapshot's manifest with the fresh track. `manifest_json` is the
/// stored column: None (NULL) and an empty or blank string mean no check.
/// `rerun_action_id` is the step that will be re-run (status awaiting_user or
/// awaiting_schedule resume AT it), excluded from the "used" set.
pub fn check(
    manifest_json: Option<&str>,
    fresh: &PersonalTrack,
    completed: &TaskTrack,
    buffers: &SharedStateTrack,
    rerun_action_id: Option<&str>,
) -> Staleness {
    let Some(raw) = manifest_json.map(str::trim).filter(|s| !s.is_empty()) else {
        return Staleness::Fresh;
    };
    // Must be a JSON OBJECT: serde would also accept an empty array for a struct.
    let manifest = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => match serde_json::from_value::<ManifestFields>(v) {
            Ok(m) => m,
            Err(_) => return Staleness::Stale(StaleReason::ManifestUnreadable),
        },
        _ => return Staleness::Stale(StaleReason::ManifestUnreadable),
    };

    // Names whose hash differs or which are absent now. A name only in the fresh
    // track (a fact added while waiting) is not staleness: nothing completed
    // could have used it.
    let changed: BTreeSet<&str> = manifest
        .field_hashes
        .iter()
        .filter(|(name, old)| {
            fresh
                .fields()
                .get(*name)
                .map(|f| f.compute_content_hash())
                .as_deref()
                != Some(old.as_str())
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if changed.is_empty() {
        return Staleness::Fresh;
    }

    let mut used: BTreeSet<String> = BTreeSet::new();
    for step in completed.steps() {
        if Some(step.action_id.as_str()) == rerun_action_id {
            continue;
        }
        used.extend(buffers.read_disclosure_buffer(&step.action_id).into_keys());
    }
    let count = changed.iter().filter(|n| used.contains(**n)).count();
    if count == 0 {
        Staleness::Fresh
    } else {
        Staleness::Stale(StaleReason::FieldsChanged { count })
    }
}

/// items.id=888 stage 4: the plain message of a run stopped because its chat is a
/// one-time chat (or its latch cannot be read).
pub const ONE_TIME_CHAT_NOT_RESUMED: &str = "a one-time chat cannot be resumed";

/// The one helper every resume path calls when rehydrate_focus_run failed:
/// for ResumeStale and (items.id=888) ResumeOneTimeChat it cancels the run (only
/// if it is still parked: the same guarded UPDATE as the expiry sweep) with that
/// error's plain message and emits "run-status-update"; any other error does
/// nothing here. Returns true when the run was cancelled.
pub async fn cancel_stale_resume(
    err: &crate::conductor::lifecycle::LifecycleError,
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) -> bool {
    use crate::conductor::lifecycle::LifecycleError;
    let message = match err {
        LifecycleError::ResumeStale(_) => PERSONAL_CHANGED_WHILE_WAITING,
        LifecycleError::ResumeOneTimeChat => ONE_TIME_CHAT_NOT_RESUMED,
        _ => return false,
    };
    let cancelled = match crate::persistence::output_store::cancel_parked_run(
        user_id, persona_id, key_hex, run_id, message,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("resume_staleness: could not cancel the run: {e}");
            false
        }
    };
    if cancelled {
        if let Some(h) = app_handle {
            crate::conductor::run_retention::emit_cancelled(h, run_id, message);
        }
    }
    cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::types::{PersonalField, TaskStep};

    fn field(name: &str, value: &str) -> PersonalField {
        PersonalField {
            field_name: name.to_owned(),
            field_value: value.to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: 2,
            source_id: "t".to_owned(),
            abstraction_tier2: "omit".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }
    }

    fn track(fields: &[(&str, &str)]) -> PersonalTrack {
        let mut t = PersonalTrack::new();
        for (n, v) in fields {
            t.add_field(field(n, v)).unwrap();
        }
        t.seal();
        t
    }

    fn manifest_of(t: &PersonalTrack) -> String {
        let hashes: HashMap<String, String> = t
            .fields()
            .iter()
            .map(|(n, f)| (n.clone(), f.compute_content_hash()))
            .collect();
        serde_json::json!({
            "field_names": [],
            "field_hashes": hashes,
            "source_versions": {"guide": "1"},
            "snapshot_taken_at": "x",
        })
        .to_string()
    }

    fn step(id: &str) -> TaskStep {
        TaskStep {
            action_id: id.to_owned(),
            output_var: None,
            content: "out".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        }
    }

    /// Completed steps `ids`, where step `draft` was approved the given fields.
    fn run_state(ids: &[&str], buffered: &[(&str, &[&str])]) -> (TaskTrack, SharedStateTrack) {
        let mut tt = TaskTrack::new();
        for id in ids {
            tt.add_step(step(id));
        }
        let mut ss = SharedStateTrack::new();
        for (id, names) in buffered {
            ss.write_disclosure_buffer(
                *id,
                names
                    .iter()
                    .map(|n| ((*n).to_owned(), "v".to_owned()))
                    .collect(),
            );
        }
        (tt, ss)
    }

    const START: &[(&str, &str)] = &[("employer", "Acme"), ("job_title", "Cook")];

    #[test]
    fn an_unchanged_manifest_is_fresh() {
        let m = manifest_of(&track(START));
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(
            check(Some(&m), &track(START), &tt, &ss, None),
            Staleness::Fresh
        );
    }

    #[test]
    fn a_changed_field_a_completed_step_used_is_stale() {
        let m = manifest_of(&track(START));
        let now = track(&[("employer", "Globex"), ("job_title", "Cook")]);
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(
            check(Some(&m), &now, &tt, &ss, None),
            Staleness::Stale(StaleReason::FieldsChanged { count: 1 })
        );
    }

    #[test]
    fn a_deleted_field_a_completed_step_used_is_stale() {
        let m = manifest_of(&track(START));
        let now = track(&[("job_title", "Cook")]);
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(
            check(Some(&m), &now, &tt, &ss, None),
            Staleness::Stale(StaleReason::FieldsChanged { count: 1 })
        );
    }

    #[test]
    fn a_changed_field_no_completed_step_used_is_fresh() {
        let m = manifest_of(&track(START));
        let now = track(&[("employer", "Acme"), ("job_title", "Chef")]);
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(check(Some(&m), &now, &tt, &ss, None), Staleness::Fresh);
        // Nothing was approved to any step at all.
        let (tt, ss) = run_state(&["draft"], &[]);
        assert_eq!(check(Some(&m), &now, &tt, &ss, None), Staleness::Fresh);
    }

    #[test]
    fn an_added_field_is_fresh() {
        let m = manifest_of(&track(&[("employer", "Acme")]));
        let now = track(&[("employer", "Acme"), ("industry", "Food")]);
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer", "industry"])]);
        assert_eq!(check(Some(&m), &now, &tt, &ss, None), Staleness::Fresh);
    }

    #[test]
    fn a_source_versions_only_change_is_fresh() {
        let t = track(START);
        let m = manifest_of(&t).replace(r#""guide":"1""#, r#""guide":"0""#);
        assert!(m.contains(r#""guide":"0""#));
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(check(Some(&m), &t, &tt, &ss, None), Staleness::Fresh);
    }

    #[test]
    fn no_manifest_or_an_empty_one_means_no_check() {
        let now = track(&[("employer", "Globex")]);
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        for m in [None, Some(""), Some("   "), Some("{}")] {
            assert_eq!(check(m, &now, &tt, &ss, None), Staleness::Fresh, "{m:?}");
        }
        // A manifest from before items.id=931: the key is there, with no hashes.
        let old =
            r#"{"field_names":[],"field_hashes":{},"source_versions":{},"snapshot_taken_at":"x"}"#;
        assert_eq!(check(Some(old), &now, &tt, &ss, None), Staleness::Fresh);
    }

    #[test]
    fn an_unparseable_manifest_fails_closed() {
        let now = track(START);
        let (tt, ss) = run_state(&["draft"], &[]);
        for m in ["not json", "null", "[]", "{\"field_hashes\": 3}", "{"] {
            assert_eq!(
                check(Some(m), &now, &tt, &ss, None),
                Staleness::Stale(StaleReason::ManifestUnreadable),
                "{m}"
            );
        }
    }

    /// The buffer is the record of what was approved for a step, whether or not
    /// the step's current definition still declares the field: nothing here reads
    /// a definition, and an approved field the step never declared still counts.
    #[test]
    fn an_approved_but_undeclared_field_counts() {
        let m = manifest_of(&track(START));
        let now = track(&[("employer", "Acme"), ("job_title", "Chef")]);
        // `draft` declares nothing (no definition is even passed), yet its buffer
        // holds job_title.
        let (tt, ss) = run_state(&["draft"], &[("draft", &["job_title"])]);
        assert_eq!(
            check(Some(&m), &now, &tt, &ss, None),
            Staleness::Stale(StaleReason::FieldsChanged { count: 1 })
        );
    }

    #[test]
    fn the_step_being_rerun_and_unfinished_steps_are_excluded() {
        let m = manifest_of(&track(START));
        let now = track(&[("employer", "Globex"), ("job_title", "Cook")]);
        // The buffer names employer for `draft`, which is the step re-run: excluded.
        let (tt, ss) = run_state(&["draft"], &[("draft", &["employer"])]);
        assert_eq!(
            check(Some(&m), &now, &tt, &ss, Some("draft")),
            Staleness::Fresh
        );
        // A buffer for a step that never completed (not in the task track) is not used.
        let (tt, ss) = run_state(&["a"], &[("later", &["employer"])]);
        assert_eq!(check(Some(&m), &now, &tt, &ss, None), Staleness::Fresh);
    }

    #[test]
    fn matches_has_no_caller_outside_its_own_tests() {
        // PersonalContextManifest::matches() is left alone (it also compares
        // source_versions); this check does not use it.
        let live = include_str!("resume_staleness.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!live.contains(".matches(&"));
        assert!(!live.contains("source_versions()"));
    }
}

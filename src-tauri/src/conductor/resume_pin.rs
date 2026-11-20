// src-tauri/src/conductor/resume_pin.rs
//
// items.id=888 stage 5 (decisions.id=939 sections 6 and 7): what a checkpoint pins of
// the chat's STORES, and the check a resume makes against it.
//
// A pass seeds its store-backed bound names once, at pass start (conductor::seeding).
// A run that parks and resumes later reads them again, and the stored values may have
// changed meanwhile (another pass of the chat wrote a session variable, replaced an
// artifact, the user changed a launch answer). The run's completed steps were computed
// from the OLD values, so a resume on changed state is refused, the same way
// resume_staleness refuses a resume on a changed personal field.
//
// SEED VERSIONS. At every checkpoint (FocusRun::write_checkpoint_for) the snapshot's
// extra_metadata records, for each VERSIONED bound name the Focus's steps read (a session
// name other than the latch, a launch name, an artifact), the row's version in this chat
// and Focus, 0 for no row. VALUES ARE NEVER RECORDED: only names and integers. A store
// that cannot be read at checkpoint time records `seed_versions_unreadable: true`, and a
// resume of that snapshot is refused (fail closed). Derived names (case and delta texts,
// the device names) have no version and are not pinned.
//
// PUBLISHED IDS. The ids store writers published this pass (FocusRun.store_vars: an
// entity id, a case id, a delta id or a hand-off draft handle, opaque, never content) are
// recorded too, so a resumed run gets them back instead of failing at the next writer
// that names one. Each is RE-VERIFIED on resume against its own store, scoped to the
// running Focus (an active owned entity, a case or a delta this Focus recorded, a draft
// handle that resolves among its replies); one that cannot be verified refuses the
// resume (decisions ruling 9 of the items.id=888 plan, fail closed).
//
// A snapshot with no extra_metadata (the default '{}', every row from before this item)
// has nothing pinned and is not checked. Metadata that does not parse is refused.

use std::collections::{BTreeMap, HashMap};

use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{self as spec, Source, WRITE_MODE};
use crate::conductor::store_op::Op;
use crate::conductor::tokens::ActionDefinition;

/// A pin that cannot be read, parsed or checked. Carries nothing: the caller fails
/// closed whatever the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinError;

pub const SEED_VERSIONS_KEY: &str = "seed_versions";
pub const SEED_VERSIONS_UNREADABLE_KEY: &str = "seed_versions_unreadable";
pub const PUBLISHED_KEY: &str = "published";

/// The plain message of a run stopped because the chat's saved state changed.
pub const SAVED_STATE_CHANGED: &str = "saved chat state changed while waiting";

/// The versioned bound names these steps read: session names other than the latch,
/// launch names and artifacts. Derived names have no version.
pub fn versioned_names(steps: &[ActionDefinition]) -> Vec<&'static str> {
    crate::conductor::seeding::required_names(steps)
        .into_iter()
        .filter(|n| *n != WRITE_MODE)
        .filter(|n| {
            spec::spec(n).is_some_and(|s| {
                matches!(
                    s.source,
                    Source::Session | Source::Launch | Source::Artifact
                )
            })
        })
        .collect()
}

/// The current version of each name in this chat and Focus, 0 for no row. A run with no
/// chat has nothing stored: an empty map. Any read error, and a name that is not a
/// versioned bound name, is an Err (the caller fails closed).
pub async fn read_versions(
    conn: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    names: &[&str],
) -> Result<BTreeMap<String, i64>, PinError> {
    let mut out = BTreeMap::new();
    let Some(key) = context_key else {
        return Ok(out);
    };
    for name in names {
        let s = spec::spec(name).ok_or(PinError)?;
        let version = match s.source {
            Source::Session | Source::Launch if *name != WRITE_MODE => {
                crate::persistence::session_store::get_session_variable(conn, key, focus_id, name)
                    .await
                    .map_err(|_| PinError)?
                    .map_or(0, |v| v.version)
            }
            Source::Artifact => {
                let kind = crate::persistence::pass_artifact_store::ArtifactKind::parse(name)
                    .ok_or(PinError)?;
                crate::persistence::pass_artifact_store::get_artifact(conn, key, focus_id, kind)
                    .await
                    .map_err(|_| PinError)?
                    .map_or(0, |a| a.version)
            }
            _ => return Err(PinError),
        };
        out.insert((*name).to_owned(), version);
    }
    Ok(out)
}

/// What a snapshot pinned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pinned {
    pub versions: Option<BTreeMap<String, i64>>,
    pub unreadable: bool,
    pub published: BTreeMap<String, String>,
}

/// The snapshot's extra_metadata JSON for these versions and published ids. `versions`
/// is None when the steps read no versioned name, Some(Err) when the store could not be
/// read.
pub fn checkpoint_metadata(
    versions: Option<Result<BTreeMap<String, i64>, PinError>>,
    published: &HashMap<String, String>,
) -> String {
    let mut obj = serde_json::Map::new();
    match versions {
        None => {}
        Some(Ok(v)) => {
            obj.insert(
                SEED_VERSIONS_KEY.to_owned(),
                serde_json::to_value(v).unwrap_or_default(),
            );
        }
        Some(Err(PinError)) => {
            obj.insert(
                SEED_VERSIONS_UNREADABLE_KEY.to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if !published.is_empty() {
        let sorted: BTreeMap<&String, &String> = published.iter().collect();
        obj.insert(
            PUBLISHED_KEY.to_owned(),
            serde_json::to_value(sorted).unwrap_or_default(),
        );
    }
    serde_json::Value::Object(obj).to_string()
}

/// Parse a snapshot's extra_metadata. None, '' and '{}' pin nothing. Anything that is
/// not an object of the expected shapes is an Err (the resume is refused).
pub fn parse(extra: Option<&str>) -> Result<Pinned, PinError> {
    let text = extra.map(str::trim).unwrap_or("");
    if text.is_empty() {
        return Ok(Pinned::default());
    }
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| PinError)?;
    let obj = value.as_object().ok_or(PinError)?;
    let mut pinned = Pinned::default();
    if let Some(v) = obj.get(SEED_VERSIONS_KEY) {
        let map = v.as_object().ok_or(PinError)?;
        let mut versions = BTreeMap::new();
        for (name, n) in map {
            versions.insert(name.clone(), n.as_i64().ok_or(PinError)?);
        }
        pinned.versions = Some(versions);
    }
    if let Some(v) = obj.get(SEED_VERSIONS_UNREADABLE_KEY) {
        pinned.unreadable = v.as_bool().ok_or(PinError)?;
    }
    if let Some(v) = obj.get(PUBLISHED_KEY) {
        let map = v.as_object().ok_or(PinError)?;
        for (var, id) in map {
            pinned
                .published
                .insert(var.clone(), id.as_str().ok_or(PinError)?.to_owned());
        }
    }
    Ok(pinned)
}

/// Re-verify every published id against its own store, scoped to `focus_id`. The kind of
/// each id is the op of the row that publishes its variable. True only if every id is
/// still there; a variable no row publishes, an unknown kind, an entity id with no
/// personal.db connection, or any read error is false.
pub async fn verify_published(
    outputs: &mut SqliteConnection,
    personal: Option<&mut SqliteConnection>,
    focus_id: &str,
    steps: &[ActionDefinition],
    published: &BTreeMap<String, String>,
) -> bool {
    let mut personal = personal;
    for (var, id) in published {
        let ok = match publishing_op(steps, var) {
            Some(Op::EntityFindOrCreate) => match personal.as_deref_mut() {
                Some(conn) => {
                    crate::persistence::owned_entity_store::get_owned_active(conn, focus_id, id)
                        .await
                        .is_ok()
                }
                None => false,
            },
            Some(Op::CaseOpen) => crate::persistence::case_store::get_case(outputs, id)
                .await
                .ok()
                .flatten()
                .is_some_and(|c| c.focus_id == focus_id),
            Some(Op::DeltaRecord) => sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM config_deltas WHERE delta_id = ? AND focus_id = ?",
            )
            .bind(id)
            .bind(focus_id)
            .fetch_optional(&mut *outputs)
            .await
            .ok()
            .flatten()
            .is_some(),
            Some(Op::HandoffIngest) => {
                crate::persistence::handles::resolve_draft_handle(outputs, focus_id, id)
                    .await
                    .is_ok()
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Whether any published variable is an entity id (the caller then opens personal.db).
pub fn needs_personal(steps: &[ActionDefinition], published: &BTreeMap<String, String>) -> bool {
    published
        .keys()
        .any(|v| publishing_op(steps, v) == Some(Op::EntityFindOrCreate))
}

/// The op of the row that publishes `var`, if any.
pub fn publishing_op(steps: &[ActionDefinition], var: &str) -> Option<Op> {
    steps
        .iter()
        .filter_map(|s| s.store_op.as_ref())
        .find(|op| op.published_var() == Some(var))
        .map(|op| op.op)
}

#[cfg(test)]
#[path = "resume_pin_tests.rs"]
mod tests;

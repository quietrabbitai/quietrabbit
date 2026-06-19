// src-tauri/src/conductor/types.rs
//
// Context track types for the Conductor execution engine.
// Expanded from stubs — full port of conductor/context.py.
//
// Three tracks hold state during a focus run:
//   PersonalTrack    — personal fields from personal.db (sealed after INITIALIZE)
//   TaskTrack        — accumulates step outputs during execution
//   SharedStateTrack — content approved for cross-tier use via PG_GATE_3
//
// PersonalContextManifest — checkpoint metadata (field names + hashes only,
//   never field values). Used by resume logic to detect personal.db drift.
//
// INVARIANT: PersonalTrack is NEVER serialized to snapshots.
//   Re-fetched fresh from personal.db on every resume.
//   Only field names + content hashes + source versions go into manifest.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

// ---------------------------------------------------------------------------
// TrackError
// ---------------------------------------------------------------------------

/// Errors produced by PersonalTrack mutation after seal.
/// Python oracle: RuntimeError("PersonalTrack is sealed …")
#[derive(Debug, Error)]
pub enum TrackError {
    #[error("PersonalTrack is sealed — cannot modify after INITIALIZE")]
    SealedTrack,
}

// ---------------------------------------------------------------------------
// PersonalDBDecryptionError
// ---------------------------------------------------------------------------

/// SQLCipher key rejected or personal.db is corrupt.
/// Raised by personal_store::load_personal_track() when the database
/// cannot be opened with the provided key.
#[derive(Debug, Error)]
#[error("{plain_language}")]
pub struct PersonalDBDecryptionError {
    pub plain_language: String,
}

// ---------------------------------------------------------------------------
// PersonalField
// ---------------------------------------------------------------------------

/// A single personal field loaded from personal.db for this run.
/// field_value is the decrypted value — held in memory only,
/// never written to snapshots or logs.
///
/// #[serde(skip)] on field_value: this struct may be serialized for IPC
/// responses (abstracted view), but the raw value must never be included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalField {
    pub field_name: String,
    #[serde(skip)]
    pub field_value: String,        // decrypted — never serialized
    pub sensitivity: String,
    pub sensitivity_severity: i32,
    pub source_id: String,
    pub abstraction_tier2: String,
    pub abstraction_tier3: String,
}

impl PersonalField {
    /// SHA-256 of "field_name:field_value".
    /// Delimiter is colon — matches Python oracle: f"{name}:{value}".encode("utf-8")
    /// Used by PersonalContextManifest to detect value changes between snapshots
    /// without storing the value itself.
    pub fn compute_content_hash(&self) -> String {
        let payload = format!("{}:{}", self.field_name, self.field_value);
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

// ---------------------------------------------------------------------------
// PersonalTrack
// ---------------------------------------------------------------------------

// NOT derived: Serialize, Deserialize — PersonalTrack is NEVER serialized.
// INVARIANT: Re-fetched fresh from personal.db on every resume.
// Only PersonalContextManifest (field names + hashes) goes into snapshots.
/// Read-only view of personal fields for this focus run.
/// Populated at Phase 3 INITIALIZE, then sealed before execution begins.
/// NEVER serialized to snapshots. Re-fetched fresh on resume.
///
/// Build pattern:
///   let mut track = PersonalTrack::new();
///   track.add_field(f)?;          // during INITIALIZE
///   track.set_voice_profile(..)?; // during INITIALIZE
///   track.seal();                 // before Phase 4 EXECUTE
///   // track is now read-only
#[derive(Debug)]
pub struct PersonalTrack {
    fields: IndexMap<String, PersonalField>,
    voice_profile: IndexMap<String, String>,
    life_context: IndexMap<String, String>,       // legacy name — D6-323, do not rename
    source_versions: IndexMap<String, String>,
    sealed: bool,
}

impl PersonalTrack {
    pub fn new() -> Self {
        Self {
            fields: IndexMap::new(),
            voice_profile: IndexMap::new(),
            life_context: IndexMap::new(),
            source_versions: IndexMap::new(),
            sealed: false,
        }
    }

    // -- Build methods (called during INITIALIZE, before seal) ---------------

    pub fn add_field(&mut self, f: PersonalField) -> Result<(), TrackError> {
        if self.sealed {
            return Err(TrackError::SealedTrack);
        }
        self.fields.insert(f.field_name.clone(), f);
        Ok(())
    }

    pub fn set_voice_profile(
        &mut self,
        profile: IndexMap<String, String>,
    ) -> Result<(), TrackError> {
        if self.sealed {
            return Err(TrackError::SealedTrack);
        }
        self.voice_profile = profile;
        Ok(())
    }

    /// Legacy name retained per D6-323 standing rule — do not rename.
    pub fn set_life_context(
        &mut self,
        context: IndexMap<String, String>,
    ) -> Result<(), TrackError> {
        if self.sealed {
            return Err(TrackError::SealedTrack);
        }
        self.life_context = context;
        Ok(())
    }

    pub fn set_source_versions(
        &mut self,
        versions: IndexMap<String, String>,
    ) -> Result<(), TrackError> {
        if self.sealed {
            return Err(TrackError::SealedTrack);
        }
        self.source_versions = versions;
        Ok(())
    }

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    // -- Read methods (safe to call at any time) -----------------------------

    pub fn fields(&self) -> &IndexMap<String, PersonalField> {
        &self.fields
    }

    pub fn voice_profile(&self) -> &IndexMap<String, String> {
        &self.voice_profile
    }

    pub fn life_context(&self) -> &IndexMap<String, String> {
        &self.life_context
    }

    pub fn source_versions(&self) -> &IndexMap<String, String> {
        &self.source_versions
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Returns a clone of the field — prevents downstream mutation of stored
    /// value. Python oracle: copy.deepcopy(f).
    pub fn get_field(&self, field_name: &str) -> Option<PersonalField> {
        self.fields.get(field_name).cloned()
    }

    /// Returns clones of all fields for a given source.
    /// Python oracle: fields_for_source() returns deepcopy list.
    pub fn fields_for_source(&self, source_id: &str) -> Vec<PersonalField> {
        self.fields
            .values()
            .filter(|f| f.source_id == source_id)
            .cloned()
            .collect()
    }

    /// Highest sensitivity_severity across all loaded fields, or 0 if empty.
    pub fn max_sensitivity_severity(&self) -> i32 {
        self.fields
            .values()
            .map(|f| f.sensitivity_severity)
            .max()
            .unwrap_or(0)
    }
}

impl Default for PersonalTrack {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PersonalContextManifest
// ---------------------------------------------------------------------------

/// Checkpoint metadata — what was active in PersonalTrack at snapshot time.
/// Stored in focus_run_snapshots.personal_context_manifest (JSON).
/// Used during resume to detect changes in personal.db since last checkpoint.
///
/// Contains field names, SHA-256 content hashes, and source versions.
/// NEVER contains field values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalContextManifest {
    pub field_names: Vec<String>,                  // sorted — deterministic comparison
    pub field_hashes: HashMap<String, String>,     // field_name → sha256(name:value)
    pub source_versions: HashMap<String, String>,  // source_id → version string
    pub snapshot_taken_at: String,
}

impl PersonalContextManifest {
    /// Build a manifest from the current PersonalTrack.
    /// Python oracle: from_personal_track() classmethod.
    pub fn from_personal_track(track: &PersonalTrack, snapshot_taken_at: String) -> Self {
        let mut field_names: Vec<String> = track.fields().keys().cloned().collect();
        field_names.sort();

        let field_hashes: HashMap<String, String> = track
            .fields()
            .iter()
            .map(|(name, f)| (name.clone(), f.compute_content_hash()))
            .collect();

        let source_versions: HashMap<String, String> = track
            .source_versions()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Self {
            field_names,
            field_hashes,
            source_versions,
            snapshot_taken_at,
        }
    }

    /// Compare this manifest to the current PersonalTrack.
    /// Returns true only if field names, content hashes, AND source versions
    /// all match. Python oracle: matches() method.
    pub fn matches(&self, current: &PersonalTrack) -> bool {
        let mut current_names: Vec<String> = current.fields().keys().cloned().collect();
        current_names.sort();
        if current_names != self.field_names {
            return false;
        }

        let current_versions: HashMap<String, String> = current
            .source_versions()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if current_versions != self.source_versions {
            return false;
        }

        for (name, f) in current.fields().iter() {
            if self.field_hashes.get(name).map(|s| s.as_str())
                != Some(f.compute_content_hash().as_str())
            {
                return false;
            }
        }

        true
    }
}

// ---------------------------------------------------------------------------
// TaskStep
// ---------------------------------------------------------------------------

/// A single completed step's output, held in TaskTrack.
/// Immutable after creation. Python oracle: frozen=True dataclass.
///
/// output_var is Option<String> — Python oracle allows None (steps that
/// produce no named output variable). DEC-001 incorrectly used bare String.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStep {
    pub step_id: String,
    pub output_var: Option<String>,
    pub content: String,
    pub sensitivity_severity: i32,
    pub routing_tier_used: i32,
}

// ---------------------------------------------------------------------------
// TaskTrack
// ---------------------------------------------------------------------------

/// Accumulates step outputs during focus execution.
///
/// add_step() is the ONLY writer for steps, output_vars, and sensitivity_ceiling.
/// Fields are private to enforce this invariant — use read accessors.
///
/// sensitivity_ceiling: highest severity seen — escalates monotonically, never drops.
/// Initialized to 1 ("general") matching Python oracle (TaskTrack dataclass default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTrack {
    steps: Vec<TaskStep>,
    sensitivity_ceiling: i32,
    output_vars: HashMap<String, String>,
}

impl TaskTrack {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            sensitivity_ceiling: 1,
            output_vars: HashMap::new(),
        }
    }

    /// Add a completed step, update output cache and sensitivity ceiling.
    /// The ONLY writer for steps and output_vars.
    /// Python oracle: add_step()
    pub fn add_step(&mut self, step: TaskStep) {
        if let Some(ref var) = step.output_var {
            if !var.is_empty() {
                self.output_vars.insert(var.clone(), step.content.clone());
            }
        }
        if step.sensitivity_severity > self.sensitivity_ceiling {
            self.sensitivity_ceiling = step.sensitivity_severity;
        }
        self.steps.push(step);
    }

    // -- Read accessors ------------------------------------------------------

    pub fn steps(&self) -> &[TaskStep] {
        &self.steps
    }

    pub fn sensitivity_ceiling(&self) -> i32 {
        self.sensitivity_ceiling
    }

    /// O(1) lookup of a previous step's output by variable name.
    pub fn get_output(&self, output_var: &str) -> Option<&str> {
        self.output_vars.get(output_var).map(|s| s.as_str())
    }

    /// The most recent step's content, or None.
    pub fn last_output(&self) -> Option<&str> {
        self.steps.last().map(|s| s.content.as_str())
    }
}

impl Default for TaskTrack {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PromotedContentEntry
// ---------------------------------------------------------------------------

/// A single cross-tier content promotion approved by PG_GATE_3.
/// Immutable record — provides audit trail for all promotions.
/// Python oracle: frozen=True dataclass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotedContentEntry {
    pub step_id: String,
    pub content_key: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// SharedStateTrack
// ---------------------------------------------------------------------------

/// Holds content approved for cross-tier use via PG_GATE_3.
///
/// step_disclosure_buffers: written by PG_GATE_1, read by Step 8 for Tier 2+.
///   Contains abstracted field values only — raw personal values NEVER here.
///
/// promotions: append-only. PG_GATE_3 approved content entries.
///   Fields are private to enforce append-only and replacement semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedStateTrack {
    step_disclosure_buffers: HashMap<String, HashMap<String, String>>,
    promotions: Vec<PromotedContentEntry>,
}

impl SharedStateTrack {
    pub fn new() -> Self {
        Self {
            step_disclosure_buffers: HashMap::new(),
            promotions: Vec::new(),
        }
    }

    /// Write abstracted field values for a step.
    /// Called by PG_GATE_1. Raw personal values must never be passed here.
    /// REPLACEMENT semantics — overwrites any prior entry for this step_id.
    /// Python oracle: write_disclosure_buffer()
    pub fn write_disclosure_buffer(
        &mut self,
        step_id: impl Into<String>,
        approved_fields: HashMap<String, String>,
    ) {
        self.step_disclosure_buffers.insert(step_id.into(), approved_fields);
    }

    /// Read abstracted field values for a step.
    /// Returns empty map if no buffer exists for this step.
    /// NEVER falls back to PersonalTrack.
    /// Python oracle: read_disclosure_buffer()
    pub fn read_disclosure_buffer(&self, step_id: &str) -> HashMap<String, String> {
        self.step_disclosure_buffers
            .get(step_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Append a PG_GATE_3 approved promotion. Never overwrites existing entries.
    /// Python oracle: promote_content()
    pub fn promote_content(
        &mut self,
        step_id: impl Into<String>,
        content_key: impl Into<String>,
        content: impl Into<String>,
    ) {
        self.promotions.push(PromotedContentEntry {
            step_id: step_id.into(),
            content_key: content_key.into(),
            content: content.into(),
        });
    }

    /// Return the most recent promoted content for a given key, or None.
    /// Python oracle: get_promoted()
    pub fn get_promoted(&self, content_key: &str) -> Option<&str> {
        self.promotions
            .iter()
            .rev()
            .find(|e| e.content_key == content_key)
            .map(|e| e.content.as_str())
    }

    // -- Read accessors ------------------------------------------------------

    pub fn promotions(&self) -> &[PromotedContentEntry] {
        &self.promotions
    }

    pub fn buffers(&self) -> &HashMap<String, HashMap<String, String>> {
        &self.step_disclosure_buffers
    }
}

impl Default for SharedStateTrack {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_field(name: &str, value: &str, severity: i32) -> PersonalField {
        PersonalField {
            field_name: name.to_owned(),
            field_value: value.to_owned(),
            sensitivity: "personal".to_owned(),
            sensitivity_severity: severity,
            source_id: "personal-specialist".to_owned(),
            abstraction_tier2: "pass".to_owned(),
            abstraction_tier3: "omit".to_owned(),
        }
    }

    // -- PersonalField -------------------------------------------------------

    #[test]
    fn compute_content_hash_deterministic() {
        let f = make_field("city", "Portland", 2);
        assert_eq!(f.compute_content_hash(), f.compute_content_hash());
    }

    #[test]
    fn compute_content_hash_changes_with_value() {
        let f1 = make_field("city", "Portland", 2);
        let f2 = make_field("city", "Seattle", 2);
        assert_ne!(f1.compute_content_hash(), f2.compute_content_hash());
    }

    // -- PersonalTrack -------------------------------------------------------

    #[test]
    fn personal_track_add_and_seal() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        assert!(!track.is_sealed());
        track.seal();
        assert!(track.is_sealed());
    }

    #[test]
    fn personal_track_sealed_rejects_add() {
        let mut track = PersonalTrack::new();
        track.seal();
        let err = track.add_field(make_field("city", "Portland", 2)).unwrap_err();
        assert!(err.to_string().contains("sealed"));
    }

    #[test]
    fn personal_track_sealed_rejects_set_voice_profile() {
        let mut track = PersonalTrack::new();
        track.seal();
        assert!(track.set_voice_profile(IndexMap::new()).is_err());
    }

    #[test]
    fn personal_track_max_sensitivity() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("name", "Alice", 2)).unwrap();
        track.add_field(make_field("diagnosis", "X", 3)).unwrap();
        assert_eq!(track.max_sensitivity_severity(), 3);
    }

    #[test]
    fn personal_track_max_sensitivity_empty() {
        let track = PersonalTrack::new();
        assert_eq!(track.max_sensitivity_severity(), 0);
    }

    #[test]
    fn personal_track_get_field_returns_clone() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        let f = track.get_field("city").unwrap();
        assert_eq!(f.field_value, "Portland");
        let f2 = track.get_field("city").unwrap();
        assert_eq!(f.field_name, f2.field_name);
    }

    #[test]
    fn personal_track_fields_for_source() {
        let mut track = PersonalTrack::new();
        let mut f2 = make_field("age", "35", 2);
        f2.source_id = "other-source".to_owned();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        track.add_field(f2).unwrap();
        let results = track.fields_for_source("personal-specialist");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].field_name, "city");
    }

    // -- PersonalContextManifest ---------------------------------------------

    #[test]
    fn manifest_matches_identical_track() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        let manifest = PersonalContextManifest::from_personal_track(
            &track, "2026-06-19T00:00:00Z".to_owned(),
        );
        assert!(manifest.matches(&track));
    }

    #[test]
    fn manifest_detects_value_change() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        let manifest = PersonalContextManifest::from_personal_track(
            &track, "2026-06-19T00:00:00Z".to_owned(),
        );
        let mut track2 = PersonalTrack::new();
        track2.add_field(make_field("city", "Seattle", 2)).unwrap();
        assert!(!manifest.matches(&track2));
    }

    #[test]
    fn manifest_detects_field_added() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        let manifest = PersonalContextManifest::from_personal_track(
            &track, "2026-06-19T00:00:00Z".to_owned(),
        );
        let mut track2 = PersonalTrack::new();
        track2.add_field(make_field("city", "Portland", 2)).unwrap();
        track2.add_field(make_field("age", "35", 2)).unwrap();
        assert!(!manifest.matches(&track2));
    }

    #[test]
    fn manifest_detects_source_versions_change() {
        let mut track = PersonalTrack::new();
        track.add_field(make_field("city", "Portland", 2)).unwrap();
        let mut versions = IndexMap::new();
        versions.insert("personal-specialist".to_owned(), "v1".to_owned());
        track.set_source_versions(versions).unwrap();
        let manifest = PersonalContextManifest::from_personal_track(
            &track, "2026-06-19T00:00:00Z".to_owned(),
        );
        let mut track2 = PersonalTrack::new();
        track2.add_field(make_field("city", "Portland", 2)).unwrap();
        let mut versions2 = IndexMap::new();
        versions2.insert("personal-specialist".to_owned(), "v2".to_owned());
        track2.set_source_versions(versions2).unwrap();
        assert!(!manifest.matches(&track2));
    }

    // -- TaskTrack -----------------------------------------------------------

    #[test]
    fn task_track_initial_ceiling() {
        let tt = TaskTrack::new();
        assert_eq!(tt.sensitivity_ceiling(), 1);
    }

    #[test]
    fn task_track_add_step_updates_ceiling() {
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s1".to_owned(),
            output_var: Some("draft".to_owned()),
            content: "hello".to_owned(),
            sensitivity_severity: 3,
            routing_tier_used: 2,
        });
        assert_eq!(tt.sensitivity_ceiling(), 3);
    }

    #[test]
    fn task_track_ceiling_monotonic() {
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s1".to_owned(),
            output_var: None,
            content: "x".to_owned(),
            sensitivity_severity: 3,
            routing_tier_used: 1,
        });
        tt.add_step(TaskStep {
            step_id: "s2".to_owned(),
            output_var: None,
            content: "y".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
        });
        assert_eq!(tt.sensitivity_ceiling(), 3);
    }

    #[test]
    fn task_track_get_output_and_last() {
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s1".to_owned(),
            output_var: Some("draft".to_owned()),
            content: "hello world".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
        });
        assert_eq!(tt.get_output("draft"), Some("hello world"));
        assert_eq!(tt.last_output(), Some("hello world"));
        assert_eq!(tt.get_output("nonexistent"), None);
    }

    #[test]
    fn task_track_no_output_var_skips_cache() {
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s1".to_owned(),
            output_var: None,
            content: "internal".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
        });
        assert_eq!(tt.steps().len(), 1);
        assert_eq!(tt.get_output("anything"), None);
    }

    #[test]
    fn task_track_empty_string_output_var_skips_cache() {
        // Some("") is treated as no output var — must not insert empty key
        let mut tt = TaskTrack::new();
        tt.add_step(TaskStep {
            step_id: "s1".to_owned(),
            output_var: Some(String::new()),
            content: "something".to_owned(),
            sensitivity_severity: 1,
            routing_tier_used: 1,
        });
        assert_eq!(tt.steps().len(), 1);
        assert_eq!(tt.get_output(""), None);
    }

    // -- SharedStateTrack ----------------------------------------------------

    #[test]
    fn disclosure_buffer_write_and_read() {
        let mut sst = SharedStateTrack::new();
        let mut fields = HashMap::new();
        fields.insert("city".to_owned(), "Pacific Northwest".to_owned());
        sst.write_disclosure_buffer("step-1", fields);
        let buf = sst.read_disclosure_buffer("step-1");
        assert_eq!(buf.get("city").map(|s| s.as_str()), Some("Pacific Northwest"));
    }

    #[test]
    fn disclosure_buffer_missing_returns_empty() {
        let sst = SharedStateTrack::new();
        assert!(sst.read_disclosure_buffer("nonexistent").is_empty());
    }

    #[test]
    fn disclosure_buffer_replacement_semantics() {
        let mut sst = SharedStateTrack::new();
        let mut f1 = HashMap::new();
        f1.insert("city".to_owned(), "Portland".to_owned());
        sst.write_disclosure_buffer("step-1", f1);
        let mut f2 = HashMap::new();
        f2.insert("city".to_owned(), "Seattle".to_owned());
        sst.write_disclosure_buffer("step-1", f2);
        assert_eq!(
            sst.read_disclosure_buffer("step-1")
                .get("city")
                .map(|s| s.as_str()),
            Some("Seattle"),
        );
    }

    #[test]
    fn promotions_append_only_and_get_latest() {
        let mut sst = SharedStateTrack::new();
        sst.promote_content("s1", "summary", "first version");
        sst.promote_content("s2", "summary", "second version");
        assert_eq!(sst.get_promoted("summary"), Some("second version"));
        assert_eq!(sst.promotions().len(), 2);
    }

    #[test]
    fn promotions_different_key_no_interference() {
        let mut sst = SharedStateTrack::new();
        sst.promote_content("s1", "summary", "the summary");
        sst.promote_content("s1", "outline", "the outline");
        assert_eq!(sst.get_promoted("summary"), Some("the summary"));
        assert_eq!(sst.get_promoted("outline"), Some("the outline"));
    }

    #[test]
    fn promotions_missing_key_returns_none() {
        let sst = SharedStateTrack::new();
        assert_eq!(sst.get_promoted("nonexistent"), None);
    }
}

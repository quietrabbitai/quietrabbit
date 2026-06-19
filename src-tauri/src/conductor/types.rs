// src-tauri/src/conductor/types.rs
//
// Minimal stubs for conductor context types needed by persistence stores.
// Full conductor module will be ported in a later migration session.
//
// PersonalField and PersonalTrack are defined here so that
// persistence/personal_store.rs can be ported without the full
// conductor layer. These stubs match the Python semantics exactly:
//   - PersonalField: read-only data holder, never serialized to snapshots
//   - PersonalTrack: sealed after INITIALIZE, mutation raises error
//   - PersonalDBDecryptionError: SQLCipher key rejection or file corruption

use thiserror::Error;

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
#[derive(Debug, Clone)]
pub struct PersonalField {
    pub field_name: String,
    pub field_value: String,
    pub sensitivity: String,
    pub sensitivity_severity: i32,
    pub source_id: String,
    pub abstraction_tier2: String,
    pub abstraction_tier3: String,
}

impl PersonalField {
    /// SHA-256 hash of field_name:field_value.
    /// Used by PersonalContextManifest to detect value changes between
    /// snapshots without storing the value itself.
    /// STUB: returns placeholder — replace with sha2 crate when full
    /// conductor is ported. Not called by personal_store.rs directly.
    pub fn compute_content_hash(&self) -> String {
        let _ = &self.field_name;
        let _ = &self.field_value;
        "stub-hash-replace-in-conductor-port".to_owned()
    }
}

// ---------------------------------------------------------------------------
// PersonalTrack
// ---------------------------------------------------------------------------

/// Read-only view of personal fields for this focus run.
/// Populated at Phase 3 INITIALIZE, then sealed before execution begins.
/// NEVER serialized to snapshots. Re-fetched fresh on resume.
pub struct PersonalTrack {
    fields: indexmap::IndexMap<String, PersonalField>,
    voice_profile: indexmap::IndexMap<String, String>,
    life_context: indexmap::IndexMap<String, String>,
    sealed: bool,
}

impl PersonalTrack {
    pub fn new() -> Self {
        Self {
            fields: indexmap::IndexMap::new(),
            voice_profile: indexmap::IndexMap::new(),
            life_context: indexmap::IndexMap::new(),
            sealed: false,
        }
    }

    pub fn add_field(&mut self, f: PersonalField) -> Result<(), String> {
        if self.sealed {
            return Err(
                "PersonalTrack is sealed — cannot modify after INITIALIZE"
                    .to_owned(),
            );
        }
        self.fields.insert(f.field_name.clone(), f);
        Ok(())
    }

    pub fn set_voice_profile(
        &mut self,
        profile: indexmap::IndexMap<String, String>,
    ) -> Result<(), String> {
        if self.sealed {
            return Err("PersonalTrack is sealed".to_owned());
        }
        self.voice_profile = profile;
        Ok(())
    }

    /// Legacy name retained per standing rule (Chat-PM: do not rename).
    pub fn set_life_context(
        &mut self,
        context: indexmap::IndexMap<String, String>,
    ) -> Result<(), String> {
        if self.sealed {
            return Err("PersonalTrack is sealed".to_owned());
        }
        self.life_context = context;
        Ok(())
    }

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn fields(&self) -> &indexmap::IndexMap<String, PersonalField> {
        &self.fields
    }

    pub fn voice_profile(&self) -> &indexmap::IndexMap<String, String> {
        &self.voice_profile
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
}

impl Default for PersonalTrack {
    fn default() -> Self {
        Self::new()
    }
}

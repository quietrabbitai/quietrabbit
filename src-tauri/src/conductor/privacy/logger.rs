// src-tauri/src/conductor/privacy/logger.rs
//
// DisclosureLogger trait + NoopLogger + TestLogger + FailLogger.
// The fatality split (non-fatal at tier 1, fatal at tier 2+) lives in the
// gate functions, not here. This trait always returns a Result — gates decide
// whether to propagate or swallow based on execution_tier.

use async_trait::async_trait;
use indexmap::IndexMap;
use std::sync::Mutex;
use uuid::Uuid;

use super::errors::DisclosureLogWriteError;

/// Full disclosure log entry — mirrors Python's _write_disclosure_log parameters.
/// abstraction_tier is Option<u8>: None for gates 2, 3, 4.
/// fields_abstracted is IndexMap<String, String>: name -> abstracted value (ordered).
/// event_type carried explicitly (stored in extra_metadata JSON in Python).
#[derive(Debug, Clone)]
pub struct DisclosureLogEntry {
    pub step_id:           String,
    pub focus_run_id:      String,
    pub execution_tier:    u8,
    pub abstraction_tier:  Option<u8>,
    pub provider:          Option<String>,
    pub fields_shared:     Vec<String>,
    pub fields_abstracted: IndexMap<String, String>,
    pub fields_withheld:   Vec<String>,
    pub override_declined: bool,
    pub event_type:        String,
}

#[async_trait]
pub trait DisclosureLogger: Send + Sync {
    /// Write a disclosure log entry. Returns the log entry id on success.
    /// Always returns a Result — gate functions decide fatality based on
    /// execution_tier (non-fatal at tier 1, fatal at tier 2+).
    async fn write(
        &self,
        entry: DisclosureLogEntry,
    ) -> Result<String, DisclosureLogWriteError>;
}

// -- NoopLogger ---------------------------------------------------------------
// Used before the SQLCipher persistence layer is ported.
// Always succeeds. Never persists anything.
pub struct NoopLogger;

#[async_trait]
impl DisclosureLogger for NoopLogger {
    async fn write(&self, _entry: DisclosureLogEntry) -> Result<String, DisclosureLogWriteError> {
        Ok("noop".to_string())
    }
}

// -- TestLogger ---------------------------------------------------------------
// Used by golden-vector tests. Records full entry contents in insertion order.
// Assert on entry_count(), entries()[i].fields_shared, entries()[i].event_type, etc.
pub struct TestLogger {
    pub writes: Mutex<Vec<DisclosureLogEntry>>,
}

impl TestLogger {
    #[allow(clippy::new_without_default)] // Test helper; Default not meaningful.
    pub fn new() -> Self {
        Self { writes: Mutex::new(Vec::new()) }
    }

    pub fn entry_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    pub fn entries(&self) -> Vec<DisclosureLogEntry> {
        self.writes.lock().unwrap().clone()
    }
}

#[async_trait]
impl DisclosureLogger for TestLogger {
    async fn write(&self, entry: DisclosureLogEntry) -> Result<String, DisclosureLogWriteError> {
        let id = Uuid::new_v4().to_string();
        self.writes.lock().unwrap().push(entry);
        Ok(id)
    }
}

// -- FailLogger ---------------------------------------------------------------
// Used by golden-vector tests for disclosure-log failure path verification.
// Always returns Err. Gates apply fatality split based on execution_tier:
//   tier 1 -> non-fatal (gate swallows error, returns empty log id)
//   tier 2+ -> fatal (gate propagates DisclosureLogWriteError, run halts)
pub struct FailLogger;

#[async_trait]
impl DisclosureLogger for FailLogger {
    async fn write(&self, _entry: DisclosureLogEntry) -> Result<String, DisclosureLogWriteError> {
        Err(DisclosureLogWriteError::new(std::io::Error::other(
            "file is not a database",
        )))
    }
}

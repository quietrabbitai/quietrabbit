// src-tauri/src/providers/utils.rs
//
// Minimal stub for the providers::utils module.
// Provides now() — RFC3339 UTC timestamp used across persistence stores.
// Full providers module will be ported in a later migration session.
//
// Python oracle: datetime.now(timezone.utc).isoformat() → microsecond precision
// Rust: chrono::Utc::now().to_rfc3339() → nanosecond precision
// Both are valid ISO 8601. Difference is inconsequential for TEXT storage.

/// Return the current UTC time as an RFC3339 string.
/// Matches Python providers/utils.py now() behavior.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

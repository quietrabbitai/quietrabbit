// src-tauri/src/commands/auth.rs
//
// Group 11 — Auth [STUB].
// Commands: login, logout, get_recovery_key_display.
//
// Auth layer not yet ported (Layer 8). All three return not_implemented.
//
// login: establishes session, decrypts master key, populates InMemoryKeyRegistry.
// logout: clears session and key registry.
// get_recovery_key_display: derives BIP39 mnemonic from master key for one-time
//   display only. Mnemonic is never stored (Section 8.6).

#[tauri::command]
pub async fn login(_password: String) -> Result<(), String> {
    Err("not_implemented".to_string())
}

#[tauri::command]
pub async fn logout() -> Result<(), String> {
    Err("not_implemented".to_string())
}

/// One-time display only — mnemonic is never stored (Section 8.6).
#[tauri::command]
pub async fn get_recovery_key_display() -> Result<String, String> {
    Err("not_implemented".to_string())
}

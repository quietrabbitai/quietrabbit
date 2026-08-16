// src-tauri/src/auth/mod.rs
//
// Auth module -- cryptographic and lifecycle logic for QR's account/session
// model (items.id=205, Architecture/AUTH_MULTIUSER_ARCHITECTURE.md).
// Deliberately separate from commands/auth.rs, which holds only the thin
// Tauri IPC surface (login/logout/get_recovery_key_display) -- same
// separation persistence/personal_store.rs already uses relative to
// commands/personal.rs.

pub mod group_invitations;
pub mod kdf;
pub mod registry;
pub mod sharing_keypair;
pub mod user_store;

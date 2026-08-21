// src-tauri/src/persona_view_sync/mod.rs
//
// VIEW-ONLY cross-account persona sharing (items.id=304, decisions.id=723):
// the read-through grant type from decisions.id=617, built as a sibling to
// persona_sync/ (SYNCED, items.id=303) rather than a branch inside it -- see
// engine.rs's own module header for the full reasoning. Reuses items.id=303's
// pending_persona_shares grant table and X25519 envelope/folder transport;
// diverges completely on accept-time behavior and on pull/apply, which is why
// this is its own module rather than an extension of persona_sync::engine.
//
// Mirrors persona_sync/'s own two-module shape (settings_store.rs +
// engine.rs) for the same reason that module's own header gives: settings
// CRUD against shared.db is a different concern from push/pull/apply logic.

pub mod engine;
pub mod settings_store;

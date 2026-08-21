// src-tauri/src/persona_sync/mod.rs
//
// Ongoing sync transport for SYNCED persona sharing (items.id=303,
// decisions.id=722): the still-open remainder of items.id=301, built on top
// of items.id=302's recipient-side materialization. See engine.rs's own
// module header for the full design -- push cadence, delivery format,
// reconciliation model, and deliberate scope boundaries.
//
// Mirrors group_sync/'s own two-module shape (settings_store.rs +
// engine.rs) for the same reason: settings CRUD against shared.db is a
// different concern from the push/pull/reconciliation logic itself, and
// group.db's folder-sync already proved this split out.

pub mod engine;
pub mod settings_store;

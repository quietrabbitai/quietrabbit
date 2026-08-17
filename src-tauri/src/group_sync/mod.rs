// src-tauri/src/group_sync/mod.rs
//
// group.db folder-sync push/pull (items.id=287, group.db 266e --
// Working/GROUP_DB_DESIGN_20260802.md Section 2.4).
//
// MODULE PLACEMENT: top-level, sibling to persistence/ and ollama_sidecar/,
// not nested under persistence/ -- this module owns real filesystem I/O
// against an external, possibly-unreachable folder (a NAS, a cloud-sync
// client's local mount, ...), not just DB access against a file this
// process fully owns. Same rationale that gave ollama_sidecar its own
// top-level module for an external-process concern rather than folding it
// into providers/.
//
// engine.rs depends on persistence::group_store (calls its existing CRUD
// for the push side, and the new apply_synced_document for the pull side).
// group_store.rs has no dependency back onto this module -- see engine.rs's
// own header for why the "save hook" lives here as a wrapper layer instead
// of inside group_store.rs itself.

pub mod engine;
pub mod settings_store;

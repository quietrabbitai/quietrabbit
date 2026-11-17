// src-tauri/src/conductor/focus_dev/mod.rs
//
// items.id=898 (decisions.id=951): the PURE half of the focus_dev tool. A Focus spec
// (checked-in JSON) and the presets it may use expand into the one row shape the
// store writes (persistence::focus_composition_store::FocusSeed), and shape checks
// run over that result. Everything here is pure: no database, no file, no network,
// no path discovery. The stage 3 half (dev-root guard, validate, reseed, SQL renderer,
// emit-migration) builds on it, and the example examples/focus_dev.rs only parses
// arguments and prints.
//
// Presets are expanded HERE, at seed time (decisions.id=942 Q7): the runtime never
// sees one. Guards stay database-only (decisions.id=938): a spec states a guard as
// the typed columns, never as an expression.
//
// Nothing outside this module, its tests and the example names it (a scan test in
// stage 3 pins that).

pub mod preset;
pub mod shape;
pub mod spec;

#[cfg(test)]
mod tests;

/// Why a spec, a preset or an expansion was refused. Messages name the row or
/// preset, never a value that could be user content (specs are shipped content).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FocusDevError {
    #[error("spec: {0}")]
    Spec(String),
    #[error("preset: {0}")]
    Preset(String),
    #[error("shape: {}", .0.join("; "))]
    Shape(Vec<String>),
}

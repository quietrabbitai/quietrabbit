// src-tauri/src/privacy_model/mod.rs
//
// Privacy Filter model setup (items.id=678, decisions.id=864): the pinned
// model, where it lives, how it is verified, how it is fetched and how the
// filter is told it can load. This commit holds the pin only; the rest of the
// module lands with the installer.

pub mod pin;

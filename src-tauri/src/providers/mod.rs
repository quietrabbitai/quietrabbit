// src-tauri/src/providers/mod.rs

pub mod errors;
pub mod evaluation;
pub mod hosted_call;
pub mod hosted_client;
#[cfg(test)]
pub(crate) mod loopback_test_server;
#[cfg(test)]
mod ocr_eval;
pub mod ollama_client;
pub mod ollama_install;
pub mod types;
pub mod utils;

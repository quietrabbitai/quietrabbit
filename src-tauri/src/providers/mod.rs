// src-tauri/src/providers/mod.rs

pub mod errors;
pub mod evaluation;
pub mod hosted_call;
pub mod hosted_client;
pub mod hosted_stream;
#[cfg(test)]
pub(crate) mod loopback_test_server;
#[cfg(test)]
mod ocr_eval;
pub mod ollama_client;
pub mod ollama_install;
pub mod sse;
pub mod types;
pub mod utils;

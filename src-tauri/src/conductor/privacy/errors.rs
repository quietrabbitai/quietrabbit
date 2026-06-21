// src-tauri/src/conductor/privacy/errors.rs

use thiserror::Error;

#[derive(Debug, Error)]
#[error("{plain_language}")]
pub struct DisclosureLogWriteError {
    pub plain_language: String,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl DisclosureLogWriteError {
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            plain_language: "Quiet Rabbit couldn't record your privacy preferences \
                             before sending data to an external service. \
                             Your data was not sent. [Try again] [Get help]"
                .to_string(),
            source: Some(Box::new(source)),
        }
    }
}

#[derive(Debug, Error)]
#[error("{plain_language}")]
pub struct PrivacyGateBlockedError {
    pub plain_language: String,
}

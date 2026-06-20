//! Provider data types shared across Conductor, Ollama, Tier 2 providers,
//! and the IPC layer.
//!
//! `GenerateRequest.stream` is always resolved by `StepExecutor` —
//! callers must not set it directly.

use serde::{Deserialize, Serialize};
use specta::Type;

// ---------------------------------------------------------------------------
// Generation types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct GenerateOptions {
    pub temperature: f64,
    pub top_p: f64,
    pub num_ctx: u32,
    pub num_predict: u32,
}

/// `stream` is resolved by `StepExecutor` — callers must not set it directly.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct GenerateRequest {
    pub model: String,
    pub prompt: String,
    pub task_type: String,
    /// Resolved by `StepExecutor`. External callers must leave this `None`.
    pub(crate) stream: Option<bool>,
    pub options: Option<GenerateOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum CompletionStatus {
    Complete,
    /// Only used in Release 2.
    Streaming,
    Cancelled,
}

impl Default for CompletionStatus {
    fn default() -> Self {
        Self::Complete
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct GenerateResponse {
    pub content: String,
    pub model: String,
    pub prompt_token_count: u32,
    pub output_token_count: u32,
    pub latency_ms: f64,
    #[serde(default)]
    pub completion_status: CompletionStatus,
}

// ---------------------------------------------------------------------------
// Health / context window
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ProviderStatus {
    Available,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ProviderHealth {
    pub provider: String,
    pub status: ProviderStatus,
    /// ISO 8601 UTC string.
    pub checked_at: String,
    pub error: Option<String>,
    #[serde(default)]
    pub available_models: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ContextWindowStatusKind {
    Ok,
    Warn,
    Exceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ContextWindowStatus {
    pub status: ContextWindowStatusKind,
    #[serde(default)]
    pub token_estimate: u32,
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub usage_fraction: f64,
    pub plain_language: Option<String>,
    pub recommended_action: Option<RecommendedAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    CompactThenEscalate,
}

// ---------------------------------------------------------------------------
// Model management
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ModelfileVersion {
    pub model_id: String,
    pub expected_version: String,
    /// `None` = Modelfile not yet applied.
    pub applied_version: Option<String>,
    pub is_current: bool,
}

// ---------------------------------------------------------------------------
// Evaluation types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedFormat {
    Prose,
    StructuredOutput,
    CodeBlock,
    ShortAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct EvaluationTask {
    pub task_type: String,
    pub prompt: String,
    pub expected_format: ExpectedFormat,
    pub latency_target_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct EvaluationResult {
    pub model_id: String,
    pub task_type: String,
    pub latency_ms: f64,
    pub format_compliant: bool,
    /// `(latency_score * 0.40) + (format_score * 0.60)`
    pub score: f64,
    /// For persistence to `model_hardware_scores`. Default 1.0.
    #[serde(default = "default_hardware_factor")]
    pub hardware_factor: f64,
    /// Raw score before hardware factor applied. Default 0.0.
    #[serde(default)]
    pub seeded_score: f64,
}

fn default_hardware_factor() -> f64 {
    1.0
}

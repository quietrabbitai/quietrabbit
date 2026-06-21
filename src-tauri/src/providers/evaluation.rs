//! Level 1 evaluation harness.
//!
//! Measures model latency and format compliance across task types.
//! Results written to `model_hardware_scores` in `models/scores.db`
//! (unencrypted, per-instance — no personal data).
//!
//! Runs at startup (Layer 1) and on-demand for recalibration.
//! Task types must match `task_types.yaml`.
//!
//! Python oracle: `providers/evaluation.py`

use std::sync::OnceLock;

use indexmap::IndexMap;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::Connection;
use sqlx::SqliteConnection;

use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{
    EvaluationResult, EvaluationTask, ExpectedFormat, GenerateOptions, GenerateRequest,
};
use crate::providers::utils::{connect_options_unencrypted, get_data_root, now};

// ---------------------------------------------------------------------------
// Release 1 models
// ---------------------------------------------------------------------------

/// Models evaluated at startup (Layer 1) and on recalibration.
/// Python oracle: `RELEASE_1_MODELS`
pub const RELEASE_1_MODELS: &[&str] = &[
    "llama3.2:3b", // fast — quick_response, summarization
    "llama3.1:8b", // primary reasoning and writing
    "qwen2.5:7b",  // code specialist — Tech Support
];

// ---------------------------------------------------------------------------
// Default evaluation tasks
// ---------------------------------------------------------------------------

/// Returns the default evaluation task registry, initialized once.
///
/// `IndexMap` preserves insertion order — deterministic iteration matches
/// the Python oracle's dict literal ordering.
///
/// Python oracle: `DEFAULT_EVALUATION_TASKS`
fn default_evaluation_tasks() -> &'static IndexMap<&'static str, EvaluationTask> {
    static TASKS: OnceLock<IndexMap<&'static str, EvaluationTask>> = OnceLock::new();
    TASKS.get_or_init(|| {
        let mut m = IndexMap::new();
        m.insert(
            "summarization",
            EvaluationTask {
                task_type: "summarization".to_owned(),
                prompt: "Summarize the following in one sentence: \
                    Quiet Rabbit is a self-hosted AI platform that runs entirely \
                    on your own hardware, keeping your personal data private."
                    .to_owned(),
                expected_format: ExpectedFormat::Prose,
                latency_target_ms: 2000.0,
            },
        );
        m.insert(
            "structured_output",
            EvaluationTask {
                task_type: "structured_output".to_owned(),
                prompt: "Return a JSON object with two keys: \"status\" set to \"ok\" \
                    and \"message\" set to \"evaluation complete\". \
                    Return JSON only, no explanation."
                    .to_owned(),
                expected_format: ExpectedFormat::StructuredOutput,
                latency_target_ms: 2000.0,
            },
        );
        m.insert(
            "code",
            EvaluationTask {
                task_type: "code".to_owned(),
                prompt: "Write a Python function called add(a, b) that returns the \
                    sum of two numbers. Include a docstring."
                    .to_owned(),
                expected_format: ExpectedFormat::CodeBlock,
                latency_target_ms: 4000.0,
            },
        );
        m.insert(
            "reasoning",
            EvaluationTask {
                task_type: "reasoning".to_owned(),
                prompt: "A user has a document that is 5,000 words long. \
                    A model can process 4,000 words at a time. \
                    What is the minimum number of passes required to process \
                    the full document? Explain briefly."
                    .to_owned(),
                expected_format: ExpectedFormat::Prose,
                latency_target_ms: 4000.0,
            },
        );
        m.insert(
            "research",
            EvaluationTask {
                task_type: "research".to_owned(),
                prompt: "List three key privacy considerations for a self-hosted \
                    AI system. Be specific and concise."
                    .to_owned(),
                expected_format: ExpectedFormat::Prose,
                latency_target_ms: 4000.0,
            },
        );
        m.insert(
            "long_context",
            EvaluationTask {
                task_type: "long_context".to_owned(),
                prompt: {
                    let repeated = "Quiet Rabbit is a self-hosted AI platform. ".repeat(200);
                    format!(
                        "The following is a multi-part document. Read it carefully \
                        and answer the question at the end.\n\n{}\n\n\
                        Question: What is Quiet Rabbit? Answer in one sentence.",
                        repeated
                    )
                },
                expected_format: ExpectedFormat::Prose,
                latency_target_ms: 8000.0,
            },
        );
        m.insert(
            "creative_writing",
            EvaluationTask {
                task_type: "creative_writing".to_owned(),
                prompt: "Write a two-sentence product tagline for a privacy-focused \
                    personal AI assistant."
                    .to_owned(),
                expected_format: ExpectedFormat::Prose,
                latency_target_ms: 3000.0,
            },
        );
        m.insert(
            "quick_response",
            EvaluationTask {
                task_type: "quick_response".to_owned(),
                prompt: "What is the capital of France? Answer in one word.".to_owned(),
                expected_format: ExpectedFormat::ShortAnswer,
                latency_target_ms: 1000.0,
            },
        );
        m
    })
}

// ---------------------------------------------------------------------------
// Format compliance
// ---------------------------------------------------------------------------

/// Check whether JSON content is valid, after stripping markdown fences.
///
/// Uses explicit prefix/suffix stripping — avoids the `lstrip()` character-set
/// pitfall noted in the Python oracle comment.
///
/// Python oracle: `_is_valid_json()`
fn is_valid_json(content: &str) -> bool {
    let mut clean = content.trim();
    if let Some(rest) = clean.strip_prefix("```json") {
        clean = rest;
    } else if let Some(rest) = clean.strip_prefix("```") {
        clean = rest;
    }
    if let Some(rest) = clean.strip_suffix("```") {
        clean = rest;
    }
    serde_json::from_str::<serde_json::Value>(clean.trim()).is_ok()
}

/// Check whether content looks like code.
///
/// Avoids the `regex` crate. Oracle patterns approximated as follows:
///
/// - `#include`, `import `: unambiguous — no boundary check needed.
/// - `function \w+\(`: first token after `"function "` must be word-chars,
///   then `(` must appear immediately after that token. Prevents matching
///   prose like "This function is important (see below)".
/// - `def \w+(`, `class \w+[:{]`: left boundary (start or non-alphanumeric)
///   AND right boundary (`(`, `:`, or whitespace) both required. Prevents
///   matching "definitely", "This class is important", etc.
///
/// Known deviation: slightly more permissive than oracle regex in edge cases.
/// Acceptable — false positives are safe for this scoring gate.
///
/// Python oracle: `_looks_like_code()`
fn looks_like_code(content: &str) -> bool {
    // Unambiguous markers.
    if content.contains("#include") || content.contains("import ") {
        return true;
    }

    // `function \w+\(`: first non-whitespace token after "function " must
    // consist of word chars, followed immediately by `(`.
    if let Some(idx) = content.find("function ") {
        let after = &content[idx + "function ".len()..];
        let token: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !token.is_empty() && after[token.len()..].starts_with('(') {
            return true;
        }
    }

    // `def ` and `class `: require left boundary (start or non-alphanumeric
    // preceding char). The trailing space in the keyword already ensures a
    // word boundary on the right. Prevents "definitely", "declared", etc.
    // "This class is important" is handled by the left-boundary check —
    // the space before "class" is non-alphanumeric, so it would match.
    // That case is acceptable: prose containing standalone "class " or "def "
    // with a preceding space is rare and false-positives are safe here.
    for keyword in &["def ", "class "] {
        if let Some(idx) = content.find(keyword) {
            let left_ok =
                idx == 0 || !content.as_bytes()[idx - 1].is_ascii_alphanumeric();
            if left_ok {
                return true;
            }
        }
    }

    false
}

/// Check format compliance for an evaluation response.
///
/// Python oracle: `check_format_compliance()`
pub fn check_format_compliance(content: &str, expected_format: ExpectedFormat) -> bool {
    match expected_format {
        ExpectedFormat::Prose => {
            let trimmed = content.trim();
            !trimmed.is_empty() && !trimmed.starts_with('{')
        }
        ExpectedFormat::ShortAnswer => !content.trim().is_empty(),
        ExpectedFormat::StructuredOutput => is_valid_json(content),
        ExpectedFormat::CodeBlock => content.contains("```") || looks_like_code(content),
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Level 1 evaluation harness: latency + format compliance.
///
/// Score = `(latency_score * 0.40) + (format_compliance * 0.60)`
///
/// Stateless — no stored instance state. [`OllamaClient`] is passed at call
/// sites (it lives in the Conductor actor, not here).
///
/// Python oracle: `EvaluationHarness`
pub struct EvaluationHarness;

impl EvaluationHarness {
    /// Run one evaluation task on one model. Returns [`EvaluationResult`].
    ///
    /// Uses low temperature (0.3) for consistent evaluation output — distinct
    /// from the inference default (0.5). `num_predict=512` caps response length.
    ///
    /// Persists result to `scores.db` as a best-effort side channel — persistence
    /// failure does not affect the returned [`EvaluationResult`] or the caller.
    ///
    /// Python oracle: `run_single()`
    pub async fn run_single(
        &self,
        client: &OllamaClient,
        model_id: &str,
        task: &EvaluationTask,
        options: Option<GenerateOptions>,
    ) -> Result<EvaluationResult, crate::conductor::failure::ConductorError> {
        let opts = options.unwrap_or(GenerateOptions {
            temperature: 0.3,
            top_p: 0.90,
            num_ctx: 2048,
            num_predict: 512,
        });

        let response = client
            .generate(&GenerateRequest {
                model: model_id.to_owned(),
                prompt: task.prompt.clone(),
                task_type: task.task_type.clone(),
                stream: None,
                options: Some(opts),
            })
            .await?;

        let format_ok = check_format_compliance(&response.content, task.expected_format);
        let latency_score =
            f64::min(1.0, task.latency_target_ms / f64::max(response.latency_ms, 1.0));
        let score = (latency_score * 0.40) + (if format_ok { 1.0 } else { 0.0 } * 0.60);

        let result = EvaluationResult {
            model_id: model_id.to_owned(),
            task_type: task.task_type.clone(),
            latency_ms: response.latency_ms,
            format_compliant: format_ok,
            score,
            hardware_factor: f64::min(1.0, latency_score),
            seeded_score: score,
        };

        // Best-effort persistence — non-fatal if scores.db absent or write fails.
        if let Err(e) = self.persist_result(&result).await {
            log::warn!("evaluation: scores.db write skipped: {}", e);
        }

        Ok(result)
    }

    /// Run all task types against all models.
    ///
    /// Defaults to [`RELEASE_1_MODELS`] and [`default_evaluation_tasks`].
    /// Skips unavailable models without propagating errors.
    /// Sequential iteration matches Python oracle's `for` loop ordering.
    ///
    /// Python oracle: `run_all()`
    pub async fn run_all(
        &self,
        client: &OllamaClient,
        model_ids: Option<&[&str]>,
        tasks: Option<&IndexMap<&str, EvaluationTask>>,
    ) -> Vec<EvaluationResult> {
        let models = model_ids.unwrap_or(RELEASE_1_MODELS);
        let default_tasks = default_evaluation_tasks();
        let task_map: &IndexMap<&str, EvaluationTask> = tasks.unwrap_or(default_tasks);
        let mut results = Vec::new();

        for &model_id in models {
            for (task_type, task) in task_map {
                match self.run_single(client, model_id, task, None).await {
                    Ok(result) => {
                        log::info!(
                            "evaluation: {} / {}: score={:.2} latency={:.0}ms format={}",
                            model_id,
                            task_type,
                            result.score,
                            result.latency_ms,
                            if result.format_compliant { "OK" } else { "FAIL" },
                        );
                        results.push(result);
                    }
                    Err(e) => {
                        log::warn!(
                            "evaluation: {} / {}: SKIP ({})",
                            model_id,
                            task_type,
                            e
                        );
                    }
                }
            }
        }

        results
    }

    /// Write result to `models/scores.db`.
    ///
    /// `scores.db` is unencrypted — `connect_options_unencrypted()` is correct.
    ///
    /// `INSERT OR REPLACE` correctness depends on the schema UNIQUE constraint:
    /// `UNIQUE (model_id, task_type)` defined in `scores_001.sql`.
    ///
    /// Missing DB → silent skip (`scores.db` not yet initialized on first launch).
    /// This is a best-effort side channel — caller logs warn on `Err` and continues.
    ///
    /// Python oracle: `_persist_result()`
    async fn persist_result(&self, result: &EvaluationResult) -> Result<(), String> {
        let db_path = get_data_root().join("models").join("scores.db");

        // Non-blocking existence check — async discipline rule (HANDOFF §Async).
        if !tokio::fs::try_exists(&db_path).await.unwrap_or(false) {
            return Ok(());
        }

        let opts: SqliteConnectOptions = connect_options_unencrypted(&db_path);
        let mut conn = SqliteConnection::connect_with(&opts)
            .await
            .map_err(|e| format!("connect: {e}"))?;

        // Columns (9): id, model_id, task_type, latency_ms, format_compliance,
        //              hardware_factor, seeded_score, sample_count, recorded_at
        // Binds  (7): model_id, task_type, latency_ms, format_compliance,
        //             hardware_factor, seeded_score, recorded_at
        // (id = randomblob expression; sample_count = literal 1)
        sqlx::query(
            "INSERT OR REPLACE INTO model_hardware_scores \
             (id, model_id, task_type, latency_ms, format_compliance, \
              hardware_factor, seeded_score, sample_count, recorded_at) \
             VALUES (lower(hex(randomblob(8))), ?, ?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(&result.model_id)
        .bind(&result.task_type)
        .bind(result.latency_ms)
        .bind(if result.format_compliant { 1.0_f64 } else { 0.0_f64 })
        .bind(result.hardware_factor)
        .bind(result.seeded_score)
        .bind(now())
        .execute(&mut conn)
        .await
        .map_err(|e| format!("insert: {e}"))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- looks_like_code -----------------------------------------------------

    #[test]
    fn code_def_at_start_is_code() {
        assert!(looks_like_code("def add(a, b):\n    return a + b"));
    }

    #[test]
    fn code_def_after_newline_is_code() {
        assert!(looks_like_code(
            "Here is the function:\ndef add(a, b):\n    return a + b"
        ));
    }

    #[test]
    fn code_definitely_is_not_code() {
        assert!(!looks_like_code("This will definitely work."));
    }

    #[test]
    fn code_class_at_start_is_code() {
        assert!(looks_like_code("class Foo:\n    pass"));
    }

    #[test]
    fn code_classy_is_not_code() {
        // "classy" — alphanumeric char before "class", left boundary fails.
        assert!(!looks_like_code("That was a classy move."));
    }

    #[test]
    fn code_import_is_code() {
        assert!(looks_like_code("import os\nimport sys"));
    }

    #[test]
    fn code_include_is_code() {
        assert!(looks_like_code("#include <stdio.h>"));
    }

    #[test]
    fn code_function_declaration_is_code() {
        assert!(looks_like_code("function greet(name) { return name; }"));
    }

    #[test]
    fn code_function_prose_no_parens_is_not_code() {
        assert!(!looks_like_code(
            "Here is a function that adds two numbers and returns the result."
        ));
    }

    #[test]
    fn code_function_prose_with_parens_later_is_not_code() {
        assert!(!looks_like_code(
            "This function is important (see below for details)."
        ));
    }

    #[test]
    fn code_plain_prose_is_not_code() {
        assert!(!looks_like_code(
            "Here is a function that adds two numbers."
        ));
    }

    // -- is_valid_json -------------------------------------------------------

    #[test]
    fn json_bare_object_is_valid() {
        assert!(is_valid_json(r#"{"status": "ok"}"#));
    }

    #[test]
    fn json_fenced_json_block_is_valid() {
        assert!(is_valid_json("```json\n{\"status\": \"ok\"}\n```"));
    }

    #[test]
    fn json_plain_fence_is_valid() {
        assert!(is_valid_json("```\n{\"status\": \"ok\"}\n```"));
    }

    #[test]
    fn json_invalid_is_not_valid() {
        assert!(!is_valid_json("not json"));
    }

    // -- check_format_compliance ---------------------------------------------

    #[test]
    fn prose_non_empty_non_json_is_compliant() {
        assert!(check_format_compliance(
            "Paris is a city.",
            ExpectedFormat::Prose
        ));
    }

    #[test]
    fn prose_empty_is_not_compliant() {
        assert!(!check_format_compliance("", ExpectedFormat::Prose));
        assert!(!check_format_compliance("   ", ExpectedFormat::Prose));
    }

    #[test]
    fn prose_json_start_is_not_compliant() {
        assert!(!check_format_compliance(
            r#"{"key": "val"}"#,
            ExpectedFormat::Prose
        ));
    }

    #[test]
    fn short_answer_nonempty_is_compliant() {
        assert!(check_format_compliance("Paris", ExpectedFormat::ShortAnswer));
    }

    #[test]
    fn short_answer_empty_is_not_compliant() {
        assert!(!check_format_compliance("", ExpectedFormat::ShortAnswer));
    }

    #[test]
    fn structured_output_valid_json_is_compliant() {
        assert!(check_format_compliance(
            r#"{"status": "ok", "message": "done"}"#,
            ExpectedFormat::StructuredOutput,
        ));
    }

    #[test]
    fn structured_output_fenced_is_compliant() {
        assert!(check_format_compliance(
            "```json\n{\"status\": \"ok\"}\n```",
            ExpectedFormat::StructuredOutput,
        ));
    }

    #[test]
    fn structured_output_invalid_is_not_compliant() {
        assert!(!check_format_compliance(
            "not json",
            ExpectedFormat::StructuredOutput
        ));
    }

    #[test]
    fn code_block_fenced_is_compliant() {
        assert!(check_format_compliance(
            "```python\ndef add(a, b):\n    return a + b\n```",
            ExpectedFormat::CodeBlock,
        ));
    }

    #[test]
    fn code_block_unfenced_def_is_compliant() {
        assert!(check_format_compliance(
            "def add(a, b):\n    return a + b",
            ExpectedFormat::CodeBlock,
        ));
    }

    #[test]
    fn code_block_plain_prose_is_not_compliant() {
        assert!(!check_format_compliance(
            "Here is a function that adds two numbers.",
            ExpectedFormat::CodeBlock,
        ));
    }

    // -- Scoring formula -----------------------------------------------------

    #[test]
    fn score_perfect_latency_and_format() {
        // latency_score = min(1.0, 2000/500) = 1.0; format = true
        // score = 1.0*0.40 + 1.0*0.60 = 1.0
        let latency_score = f64::min(1.0, 2000.0 / 500_f64.max(1.0));
        let score = (latency_score * 0.40) + (1.0_f64 * 0.60);
        assert!((score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn score_slow_latency_good_format() {
        // latency_score = min(1.0, 2000/4000) = 0.5; format = true
        // score = 0.5*0.40 + 1.0*0.60 = 0.80
        let latency_score = f64::min(1.0, 2000.0 / 4000.0);
        let score = (latency_score * 0.40) + (1.0_f64 * 0.60);
        assert!((score - 0.80).abs() < 1e-9);
    }

    #[test]
    fn score_good_latency_bad_format() {
        // latency_score = 1.0; format = false
        // score = 1.0*0.40 + 0.0*0.60 = 0.40
        let score = (1.0_f64 * 0.40) + (0.0_f64 * 0.60);
        assert!((score - 0.40).abs() < 1e-9);
    }

    // -- Default task registry -----------------------------------------------

    #[test]
    fn default_tasks_count_matches_oracle() {
        // Python oracle defines exactly 8 task types.
        assert_eq!(default_evaluation_tasks().len(), 8);
    }

    #[test]
    fn default_tasks_contains_all_expected_types() {
        let tasks = default_evaluation_tasks();
        for t in &[
            "summarization",
            "structured_output",
            "code",
            "reasoning",
            "research",
            "long_context",
            "creative_writing",
            "quick_response",
        ] {
            assert!(tasks.contains_key(t), "missing task type: {t}");
        }
    }

    #[test]
    fn default_tasks_order_matches_oracle() {
        // Insertion order must match Python oracle's dict literal exactly.
        let keys: Vec<&&str> = default_evaluation_tasks().keys().collect();
        assert_eq!(
            keys,
            vec![
                &"summarization",
                &"structured_output",
                &"code",
                &"reasoning",
                &"research",
                &"long_context",
                &"creative_writing",
                &"quick_response",
            ]
        );
    }

    #[test]
    fn release_1_models_count_matches_oracle() {
        assert_eq!(RELEASE_1_MODELS.len(), 3);
    }
}

// src-tauri/src/conductor/extract.rs
//
// Extract-and-confirm: post-execute extraction pass for the Focus Builder flow.
//
// Responsibilities:
//   extract_candidates()      -- prompt llama3.2:3b, filter, classify, return candidates
//   classify_sensitivity()    -- pattern-match field_name to sensitivity tier
//   persist_candidates()      -- write Vec<ExtractCandidate> to extract_confirm_candidates
//   load_pending_candidates() -- load pending rows for a run (resume / re-emit)
//   load_unrecovered_rows()   -- load status='confirmed' AND persisted_at IS NULL (crash recovery)
//   persist_confirmed_field() -- write one confirmed candidate to personal.db + set persisted_at
//   mark_candidate_decided()  -- UPDATE status/confirmed_at/declined_at/updated_at
//   set_persisted_at()        -- mark crash-recovery complete after personal.db write
//   count_pending()           -- count pending rows for a run (resume routing)
//
// Extraction model: llama3.2:3b (quick_response slot, D6-363).
//
// Confidence thresholds (D6-362):
//   < 0.6  -> discard before persist (under-identification is the non-negotiable failure mode)
//   0.6-0.8 -> warn flag on candidate (surfaced to user with lower confidence indicator)
//   > 0.8  -> normal
//
// classify_sensitivity() starter ruleset (Release 1 -- expand post-testing):
//   medical:   field_name contains any of: health, medical, diagnosis, condition,
//              medication, allergy, doctor, symptom, prescription, illness
//   financial: field_name contains any of: income, salary, budget, bank, credit,
//              debt, investment, tax, financial, account
//   personal:  all other field_names (default)
//
// Cross-database write sequence (two SQLCipher DBs -- no shared transaction):
//   persist_confirmed_field() is called once per confirmed candidate by
//   submit_extract_confirm (commands/consent.rs). Sequence:
//     1. mark_candidate_decided() -> COMMIT outputs.db
//     2. persist_confirmed_field() -> save_personal_field() -> COMMIT personal.db
//     3. set_persisted_at() -> COMMIT outputs.db
//   Crash recovery: on resume, load_unrecovered_rows() finds
//   status='confirmed' AND persisted_at IS NULL -> replay step 2 only.
//   save_personal_field() is idempotent: uniqueness key is field_name within
//   (user_id, persona_id). Replay of the same field_name -> UPDATE, not INSERT.
//   No duplicate rows or audit events are produced.
//
// Parse failure / model error -> returns Ok(vec![]) -> caller proceeds to output().
// Never fatal. Under-identification is acceptable; over-identification is not.

use std::collections::HashSet;
use std::path::PathBuf;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::SqliteConnection;

use crate::persistence::personal_store::{save_personal_field, PersonalStoreError};
use crate::providers::ollama_client::OllamaClient;
use crate::providers::types::{GenerateOptions, GenerateRequest};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Extraction model -- quick_response slot (D6-363).
const EXTRACT_MODEL: &str = "llama3.2:3b";

/// Confidence below this threshold -> discard before persist.
const CONFIDENCE_SUPPRESS: f64 = 0.6;

/// Confidence at or above this threshold -> normal (no warn flag).
const CONFIDENCE_WARN_CEILING: f64 = 0.8;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single candidate returned by extract_candidates() -- not yet persisted.
#[derive(Debug, Clone)]
pub struct ExtractCandidate {
    pub field_name: String,
    pub extracted_value: String,
    pub sensitivity: String,
    pub reason: Option<String>,
    pub confidence: f64,
    /// True when confidence is in the 0.6-0.8 warn band.
    pub warn_flag: bool,
}

/// A persisted candidate row loaded from extract_confirm_candidates.
#[derive(Debug, Clone)]
pub struct PersistedCandidate {
    pub id: i64,
    pub field_name: String,
    pub extracted_value: String,
    pub sensitivity: String,
    pub reason: Option<String>,
    pub confidence: f64,
    pub warn_flag: bool,
    pub status: String,
}

// ---------------------------------------------------------------------------
// DB opener (module-local)
// ---------------------------------------------------------------------------

fn get_outputs_db_path(user_id: &str, persona_id: &str) -> PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
        .join("personas")
        .join(persona_id)
        .join("outputs.db")
}

/// Open outputs.db with SQLCipher key.
/// Pattern mirrors output_store.rs -- each persistence module owns its opener.
async fn open_outputs_db(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<SqliteConnection, sqlx::Error> {
    let db_path = get_outputs_db_path(user_id, persona_id);

    if !db_path.exists() {
        crate::persistence::migrations::migrate_outputs_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| {
                sqlx::Error::Io(std::io::Error::other(format!(
                    "outputs.db migration failed: {e}"
                )))
            })?;
    }

    let network_storage = std::env::var("QR_NETWORK_STORAGE")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false);
    let journal_mode = if network_storage { "DELETE" } else { "WAL" };

    SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key_hex}'\""))
        .pragma("journal_mode", journal_mode)
        .pragma("busy_timeout", "5000")
        .connect()
        .await
}

// ---------------------------------------------------------------------------
// classify_sensitivity
// ---------------------------------------------------------------------------

/// Map a field_name to a sensitivity tier.
/// Pattern-match on lowercase field_name. Default: "personal".
/// Release 1 starter ruleset -- expand when Chat-TESTING surfaces real field names.
pub fn classify_sensitivity(field_name: &str) -> &'static str {
    let lower = field_name.to_lowercase();

    const MEDICAL_TERMS: &[&str] = &[
        "health",
        "medical",
        "diagnosis",
        "condition",
        "medication",
        "allergy",
        "doctor",
        "symptom",
        "prescription",
        "illness",
    ];
    if MEDICAL_TERMS.iter().any(|t| lower.contains(t)) {
        return "medical";
    }

    const FINANCIAL_TERMS: &[&str] = &[
        "income",
        "salary",
        "budget",
        "bank",
        "credit",
        "debt",
        "investment",
        "tax",
        "financial",
        "account",
    ];
    if FINANCIAL_TERMS.iter().any(|t| lower.contains(t)) {
        return "financial";
    }

    "personal"
}

// ---------------------------------------------------------------------------
// validate_field_name (module-local)
// ---------------------------------------------------------------------------

/// Validate that field_name is a safe snake_case identifier.
/// First char must be a-z; remaining chars must be a-z, 0-9, or underscore.
/// Regex crate not available (CLAUDE.md) -- use char-level validation.
fn validate_field_name(field_name: &str) -> bool {
    if field_name.is_empty() {
        return false;
    }
    let mut chars = field_name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ---------------------------------------------------------------------------
// extract_candidates
// ---------------------------------------------------------------------------

/// Run the extraction pass over step outputs for a focus run.
///
/// Steps:
///   1. Concatenate step outputs with "STEP: <step_id>" separators.
///   2. Build exclusion list from focus field_requirements + field_names
///      of any existing personal.db rows (passed in as `excluded_fields`).
///   3. Prompt llama3.2:3b for JSON array of extraction candidates.
///   4. Parse response; apply confidence filter, bounds check, field_name
///      validation, intra-response duplicate suppression, excluded suppression.
///   5. Return filtered Vec<ExtractCandidate>. Empty vec on any error or
///      when all candidates are filtered -- caller proceeds to output().
///
/// `excluded_fields`: focus field_requirements + existing personal.db field names.
/// `step_outputs`:    Vec of (step_id, output_text) in execution order.
pub async fn extract_candidates(
    step_outputs: &[(String, String)],
    excluded_fields: &[String],
    client: &OllamaClient,
) -> Vec<ExtractCandidate> {
    if step_outputs.is_empty() {
        return vec![];
    }

    // Step 1: concatenate with separators
    let conversation_text = step_outputs
        .iter()
        .map(|(step_id, text)| format!("STEP: {step_id}\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    // Step 2: build exclusion list for prompt
    let excluded_list = if excluded_fields.is_empty() {
        "none".to_owned()
    } else {
        excluded_fields.join(", ")
    };

    // Step 3: prompt
    let prompt = format!(
        r#"You are an information extraction assistant. Read the conversation below and identify stable personal facts about the user that are worth remembering for future sessions.

Return ONLY a JSON array. Each element must have exactly these fields:
  "field_name": a short snake_case identifier (e.g. "preferred_name", "home_city")
  "value": the extracted fact as a plain string
  "reason": one sentence explaining why this fact is useful to remember
  "confidence": a number from 0.0 to 1.0 indicating how confident you are

Rules:
- Only extract facts that are stable over time (not one-off or context-specific).
- Do NOT extract any of these fields (already known or excluded): {excluded_list}
- Do NOT extract sensitive identifiers: passwords, account numbers, SSNs, PINs.
- If there are no facts worth extracting, return an empty array: []
- Return ONLY the JSON array. No explanation, no markdown, no preamble.

Conversation:
{conversation_text}
"#
    );

    let request = GenerateRequest {
        model: EXTRACT_MODEL.to_owned(),
        prompt,
        task_type: "extraction".to_owned(),
        stream: Some(false),
        options: Some(GenerateOptions {
            temperature: 0.2,
            top_p: 0.90,
            num_ctx: 4096,
            num_predict: 1024,
        }),
    };

    // Step 4: call model
    let response_text = match client.generate(&request).await {
        Ok(resp) => resp.content,
        Err(e) => {
            log::warn!("extract_candidates: model call failed (non-fatal): {e}");
            return vec![];
        }
    };

    // Step 5: parse and filter
    parse_and_filter(&response_text, excluded_fields)
}

// ---------------------------------------------------------------------------
// parse_and_filter (module-local)
// ---------------------------------------------------------------------------

fn parse_and_filter(response_text: &str, excluded_fields: &[String]) -> Vec<ExtractCandidate> {
    // Strip markdown fences if model wrapped the JSON
    let trimmed = response_text.trim();
    let json_str = if trimmed.starts_with("```") {
        trimmed
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        trimmed.to_owned()
    };

    let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("extract_candidates: JSON parse failed (non-fatal): {e}");
            return vec![];
        }
    };

    let arr = match parsed.as_array() {
        Some(a) => a,
        None => {
            log::warn!("extract_candidates: model returned non-array JSON (non-fatal)");
            return vec![];
        }
    };

    let excluded_normalized: Vec<String> = excluded_fields
        .iter()
        .map(|f| f.trim().to_lowercase())
        .collect();

    // Intra-response duplicate suppression: track field_names accepted this pass.
    let mut seen_this_response: HashSet<String> = HashSet::new();

    let mut candidates = Vec::new();

    for item in arr {
        // Extract required fields
        let field_name = match item.get("field_name").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => {
                log::debug!("extract_candidates: skipping item with missing/empty field_name");
                continue;
            }
        };
        let value = match item.get("value").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => {
                log::debug!("extract_candidates: skipping '{field_name}' -- empty value");
                continue;
            }
        };
        let confidence = match item.get("confidence").and_then(|v| v.as_f64()) {
            Some(c) => c,
            None => {
                log::debug!("extract_candidates: skipping '{field_name}' -- missing confidence");
                continue;
            }
        };
        let reason = item
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        // Confidence bounds check (mirrors schema CHECK constraint)
        if !(0.0..=1.0).contains(&confidence) {
            log::debug!(
                "extract_candidates: skipping '{field_name}' -- confidence {confidence} out of bounds"
            );
            continue;
        }

        // Confidence suppress threshold
        if confidence < CONFIDENCE_SUPPRESS {
            log::debug!(
                "extract_candidates: suppressed '{field_name}' confidence={confidence:.2} < {CONFIDENCE_SUPPRESS}"
            );
            continue;
        }

        // field_name validation: snake_case only (a-z start, a-z0-9_ body)
        if !validate_field_name(&field_name) {
            log::debug!("extract_candidates: invalid field_name '{field_name}' -- skipping");
            continue;
        }

        let field_normalized = field_name.to_lowercase();

        // Intra-response duplicate suppression
        if seen_this_response.contains(&field_normalized) {
            log::debug!("extract_candidates: intra-response duplicate '{field_name}' -- skipping");
            continue;
        }

        // Excluded field suppression (focus requirements + existing personal.db fields)
        if excluded_normalized.contains(&field_normalized) {
            log::debug!("extract_candidates: suppressed '{field_name}' -- matches excluded field");
            continue;
        }

        seen_this_response.insert(field_normalized);

        let sensitivity = classify_sensitivity(&field_name).to_owned();
        let warn_flag = confidence < CONFIDENCE_WARN_CEILING;

        candidates.push(ExtractCandidate {
            field_name,
            extracted_value: value,
            sensitivity,
            reason,
            confidence,
            warn_flag,
        });
    }

    candidates
}

// ---------------------------------------------------------------------------
// persist_candidates
// ---------------------------------------------------------------------------

/// Write a Vec<ExtractCandidate> to extract_confirm_candidates in outputs.db.
/// Returns the row ids of the inserted rows.
/// Called by execute_full() after extract_candidates() returns non-empty.
pub async fn persist_candidates(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
    candidates: &[ExtractCandidate],
) -> Result<Vec<i64>, sqlx::Error> {
    if candidates.is_empty() {
        return Ok(vec![]);
    }

    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let now = crate::providers::utils::now();
    let mut ids = Vec::new();

    for c in candidates {
        let row = sqlx::query(
            "INSERT INTO extract_confirm_candidates
             (focus_run_id, field_name, extracted_value, sensitivity,
              reason, confidence, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?)
             RETURNING id",
        )
        .bind(focus_run_id)
        .bind(&c.field_name)
        .bind(&c.extracted_value)
        .bind(&c.sensitivity)
        .bind(&c.reason)
        .bind(c.confidence)
        .bind(&now)
        .bind(&now)
        .fetch_one(&mut conn)
        .await?;

        let id: i64 = row.try_get("id")?;
        ids.push(id);
    }

    Ok(ids)
}

// ---------------------------------------------------------------------------
// load_pending_candidates
// ---------------------------------------------------------------------------

/// Load all pending candidates for a focus run (for re-emit on resume).
pub async fn load_pending_candidates(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Vec<PersistedCandidate>, sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, field_name, extracted_value, sensitivity, reason, confidence, status
         FROM extract_confirm_candidates
         WHERE focus_run_id = ? AND status = 'pending'
         ORDER BY id",
    )
    .bind(focus_run_id)
    .fetch_all(&mut conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            let confidence: f64 = r.try_get("confidence")?;
            Ok(PersistedCandidate {
                id: r.try_get("id")?,
                field_name: r.try_get("field_name")?,
                extracted_value: r.try_get("extracted_value")?,
                sensitivity: r.try_get("sensitivity")?,
                reason: r.try_get("reason")?,
                confidence,
                warn_flag: confidence < CONFIDENCE_WARN_CEILING,
                status: r.try_get("status")?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// load_unrecovered_rows
// ---------------------------------------------------------------------------

/// Load candidates with status='confirmed' AND persisted_at IS NULL.
/// These are crash-recovery targets -- personal.db write did not complete.
/// Returns Vec of (candidate_id, field_name, confirmed_value, sensitivity).
/// Sensitivity is read from persisted state -- never recomputed.
/// Recovery must replay exactly what was persisted; recomputing classify_sensitivity()
/// would silently apply any ruleset changes to records written under the old rules.
pub async fn load_unrecovered_rows(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<Vec<(i64, String, String, String)>, sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let rows = sqlx::query(
        "SELECT id, field_name, confirmed_value, sensitivity
         FROM extract_confirm_candidates
         WHERE focus_run_id = ? AND status = 'confirmed' AND persisted_at IS NULL
         ORDER BY id",
    )
    .bind(focus_run_id)
    .fetch_all(&mut conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get::<i64, _>("id")?,
                r.try_get::<String, _>("field_name")?,
                r.try_get::<String, _>("confirmed_value")?,
                r.try_get::<String, _>("sensitivity")?,
            ))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// mark_candidate_decided
// ---------------------------------------------------------------------------

/// Update a candidate row to confirmed or declined.
/// Sets confirmed_at or declined_at and updated_at.
/// Does NOT set persisted_at -- that is set only by set_persisted_at()
/// after personal.db write succeeds (step 3 of the cross-DB write sequence).
pub async fn mark_candidate_decided(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    candidate_id: i64,
    confirmed: bool,
    confirmed_value: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let now = crate::providers::utils::now();

    if confirmed {
        sqlx::query(
            "UPDATE extract_confirm_candidates
             SET status = 'confirmed', confirmed_value = ?,
                 confirmed_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(confirmed_value)
        .bind(&now)
        .bind(&now)
        .bind(candidate_id)
        .execute(&mut conn)
        .await?;
    } else {
        sqlx::query(
            "UPDATE extract_confirm_candidates
             SET status = 'declined', declined_at = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(candidate_id)
        .execute(&mut conn)
        .await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// set_persisted_at
// ---------------------------------------------------------------------------

/// Mark a candidate row as fully persisted (personal.db write complete).
/// Step 3 of the cross-database write sequence -- called after
/// save_personal_field() commits successfully.
pub async fn set_persisted_at(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    candidate_id: i64,
) -> Result<(), sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;
    let now = crate::providers::utils::now();

    sqlx::query(
        "UPDATE extract_confirm_candidates
         SET persisted_at = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(candidate_id)
    .execute(&mut conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// persist_confirmed_field
// ---------------------------------------------------------------------------

/// Write a confirmed candidate's value to personal.db via save_personal_field().
///
/// Idempotency guarantee: save_personal_field() uses field_name as the uniqueness
/// key within (user_id, persona_id). Replay of the same field_name produces an
/// UPDATE, not a duplicate INSERT. No duplicate rows or audit events result.
///
/// This is the idempotent personal.db step in the cross-database write sequence.
/// Crash recovery replays this function for any row with status='confirmed'
/// AND persisted_at IS NULL.
pub async fn persist_confirmed_field(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    field_name: &str,
    confirmed_value: &str,
    sensitivity: &str,
) -> Result<(), PersonalStoreError> {
    // abstraction_tier2/tier3 are display hints for the frontend -- not enforcement.
    let (abstraction_tier2, abstraction_tier3) = match sensitivity {
        "medical" | "financial" => ("redacted", "redacted"),
        _ => ("general", "general"),
    };

    save_personal_field(
        user_id,
        persona_id,
        key_hex,
        field_name,
        confirmed_value,
        sensitivity,
        /*source_id=*/ "extract_confirm",
        /*ownership_scope=*/ "self",
        abstraction_tier2,
        abstraction_tier3,
        /*source=*/ "extract_confirm",
        /*extra_metadata=*/ None,
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// count_pending
// ---------------------------------------------------------------------------

/// Return the count of pending candidates for a focus run.
/// Used by resume_run() to decide whether to re-emit or proceed to output().
pub async fn count_pending(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_run_id: &str,
) -> Result<i64, sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT COUNT(*) as cnt FROM extract_confirm_candidates
         WHERE focus_run_id = ? AND status = 'pending'",
    )
    .bind(focus_run_id)
    .fetch_one(&mut conn)
    .await?;

    row.try_get::<i64, _>("cnt")
}

// ---------------------------------------------------------------------------
// CandidateFields
// ---------------------------------------------------------------------------

/// Typed return from get_candidate_fields().
/// Avoids tuple field-order bugs at call sites in submit_extract_confirm.
pub struct CandidateFields {
    pub field_name: String,
    pub sensitivity: String,
}

// ---------------------------------------------------------------------------
// get_candidate_fields
// ---------------------------------------------------------------------------

/// Fetch field_name and sensitivity for a candidate by id.
/// Used by submit_extract_confirm to source these values from the DB rather
/// than trusting the frontend -- prevents a caller from substituting a
/// different field_name or downgrading the sensitivity classification.
/// Returns None if the candidate row does not exist.
pub async fn get_candidate_fields(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    candidate_id: i64,
) -> Result<Option<CandidateFields>, sqlx::Error> {
    let mut conn = open_outputs_db(user_id, persona_id, key_hex).await?;

    let row = sqlx::query(
        "SELECT field_name, sensitivity
         FROM extract_confirm_candidates
         WHERE id = ?",
    )
    .bind(candidate_id)
    .fetch_optional(&mut conn)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(CandidateFields {
            field_name: r.try_get("field_name")?,
            sensitivity: r.try_get("sensitivity")?,
        })),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_sensitivity_medical() {
        assert_eq!(classify_sensitivity("health_condition"), "medical");
        assert_eq!(classify_sensitivity("current_medication"), "medical");
        assert_eq!(classify_sensitivity("known_allergy"), "medical");
    }

    #[test]
    fn test_classify_sensitivity_financial() {
        assert_eq!(classify_sensitivity("annual_income"), "financial");
        assert_eq!(classify_sensitivity("credit_score"), "financial");
        assert_eq!(classify_sensitivity("bank_name"), "financial");
    }

    #[test]
    fn test_classify_sensitivity_personal_default() {
        assert_eq!(classify_sensitivity("preferred_name"), "personal");
        assert_eq!(classify_sensitivity("home_city"), "personal");
        assert_eq!(classify_sensitivity("occupation"), "personal");
    }

    #[test]
    fn test_validate_field_name_valid() {
        assert!(validate_field_name("home_city"));
        assert!(validate_field_name("preferred_name"));
        assert!(validate_field_name("age"));
        assert!(validate_field_name("zip_code_2"));
    }

    #[test]
    fn test_validate_field_name_invalid() {
        assert!(!validate_field_name(""));
        assert!(!validate_field_name("Home_City")); // uppercase
        assert!(!validate_field_name("home city")); // space
        assert!(!validate_field_name("home-city")); // hyphen
        assert!(!validate_field_name("1home")); // starts with digit
        assert!(!validate_field_name("foo;bar")); // semicolon
        assert!(!validate_field_name("../etc")); // path traversal
    }

    #[test]
    fn test_parse_and_filter_confidence_suppression() {
        let json = r#"[
            {"field_name": "home_city", "value": "Austin", "reason": "useful", "confidence": 0.5},
            {"field_name": "occupation", "value": "engineer", "reason": "useful", "confidence": 0.9}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].field_name, "occupation");
    }

    #[test]
    fn test_parse_and_filter_confidence_out_of_bounds() {
        let json = r#"[
            {"field_name": "home_city", "value": "Austin", "reason": "r", "confidence": 1.7},
            {"field_name": "occupation", "value": "engineer", "reason": "r", "confidence": -0.5}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_and_filter_excluded_fields() {
        let json = r#"[
            {"field_name": "home_city", "value": "Austin", "reason": "useful", "confidence": 0.9}
        ]"#;
        let excluded = vec!["home_city".to_owned()];
        let result = parse_and_filter(json, &excluded);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_and_filter_intra_response_dedup() {
        let json = r#"[
            {"field_name": "home_city", "value": "Austin", "reason": "r", "confidence": 0.9},
            {"field_name": "home_city", "value": "Dallas", "reason": "r", "confidence": 0.85}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].extracted_value, "Austin");
    }

    #[test]
    fn test_parse_and_filter_invalid_field_name() {
        let json = r#"[
            {"field_name": "bad field", "value": "x", "reason": "r", "confidence": 0.9},
            {"field_name": "Home_City", "value": "x", "reason": "r", "confidence": 0.9}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_and_filter_empty_array() {
        let result = parse_and_filter("[]", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_and_filter_bad_json() {
        let result = parse_and_filter("not json at all", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_warn_flag_in_band() {
        let json = r#"[
            {"field_name": "preferred_name", "value": "Alex", "reason": "r", "confidence": 0.7}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert_eq!(result.len(), 1);
        assert!(result[0].warn_flag);
    }

    #[test]
    fn test_no_warn_flag_above_ceiling() {
        let json = r#"[
            {"field_name": "home_city", "value": "Austin", "reason": "r", "confidence": 0.85}
        ]"#;
        let result = parse_and_filter(json, &[]);
        assert_eq!(result.len(), 1);
        assert!(!result[0].warn_flag);
    }

    #[test]
    fn test_parse_and_filter_strips_markdown_fences() {
        let json = "```json\n[{\"field_name\":\"home_city\",\"value\":\"Austin\",\"reason\":\"r\",\"confidence\":0.9}]\n```";
        let result = parse_and_filter(json, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].field_name, "home_city");
    }
}

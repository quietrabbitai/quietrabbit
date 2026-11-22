// src-tauri/src/conductor/launch_config.rs
//
// items.id=897 stage 1 (decisions.id=942 point 8 and addendum 3 g, decisions.id=948):
// a Focus's LAUNCH QUESTIONS and MODE PROFILES, as shipped data in shared.db
// (schema/shared_047.sql), and the one function that validates them.
//
// WHAT A LAUNCH CONFIG IS. A Focus may declare up to five closed-choice questions
// asked before a run, or none. Each question owns one of five compiled slots
// (SLOTS: the bound names document_type, launch_mode, launch_answer_1 to 3), so a
// guard can read the answer. The options are closed literals. Every question ships
// a default that is one of its options (decisions.id=948 point 3), so a run that
// was never answered, or whose chat is one-time, always has a value.
//
// A MODE is an option of the one question of kind `mode` (slot launch_mode). A mode
// has a profile: an EnvelopeOverride (quality floor, highest tier, destinations,
// declared inputs). It is applied ONLY through tighten() (model_block::config), so
// a mode can only remove. Nothing here can add a destination, raise a tier or
// switch a gate off, and no launch value approves a hosted or metered step
// (decisions.id=942 Q3, which always takes one explicit plan approval).
//
// WHAT IS NOT HERE. A Focus's display name and description are read from their one
// existing home by conductor::focus_display and are never copied into these tables.
// Answers are not here either: they are session values in the persona's encrypted
// outputs.db (a later stage). shared.db is unencrypted, so these tables hold only
// shipped data identical for every install.
//
// FAIL CLOSED. A row this build cannot read (an unknown kind, a malformed envelope)
// is an error, never skipped: a launch config only ever narrows what runs, and a
// config that was half read could leave a mode's narrowing out. A Focus with no rows
// has no launch config (read returns None).
//
// WHERE validate() RUNS: LOAD (stage 2, on every Focus), the admission test (every
// shipped Focus) and the submit command (stage 4). One function, so the three can
// never disagree.

use std::collections::BTreeSet;

use sqlx::{Row, SqlitePool};

use crate::conductor::executor::find_tokens;
use crate::conductor::model_block::config::{is_slug, EnvelopeOverride};
use crate::conductor::model_block::document_type::{check_text, Registry};
use crate::conductor::tokens::ActionDefinition;

/// The five compiled slots a question may use. Each is also a bound name
/// (conductor::bound_spec, stage 2 pins the two lists equal).
pub const SLOT_DOCUMENT_TYPE: &str = "document_type";
pub const SLOT_LAUNCH_MODE: &str = "launch_mode";
pub const SLOTS: [&str; 5] = [
    SLOT_DOCUMENT_TYPE,
    SLOT_LAUNCH_MODE,
    "launch_answer_1",
    "launch_answer_2",
    "launch_answer_3",
];

/// Most questions a Focus may ask: one per slot.
pub const MAX_QUESTIONS: usize = SLOTS.len();

/// Longest question or option id, in characters.
pub const ID_MAX_CHARS: usize = 40;
/// Longest prompt, label and hint, in bytes (placeholder wording).
pub const PROMPT_MAX_BYTES: usize = 200;
pub const LABEL_MAX_BYTES: usize = 60;
pub const HINT_MAX_BYTES: usize = 160;
/// Longest one-line description, in characters (the SQL CHECK says the same).
pub const ONE_LINER_MAX_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    /// Its options are the Focus's modes. At most one per Focus, on launch_mode.
    Mode,
    Choice,
}

impl QuestionKind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "mode" => Some(Self::Mode),
            "choice" => Some(Self::Choice),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOption {
    pub id: String,
    pub sort_order: i64,
    /// Placeholder wording, Chat-BRAND's (items.id=506).
    pub label: String,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchQuestion {
    pub id: String,
    /// One of SLOTS.
    pub slot: String,
    pub kind: QuestionKind,
    pub sort_order: i64,
    /// false: a preset that fixes the value. The default is used and the question
    /// is not shown or submittable.
    pub asked: bool,
    pub default_option: String,
    pub prompt: String,
    pub options: Vec<LaunchOption>,
}

impl LaunchQuestion {
    pub fn option(&self, id: &str) -> Option<&LaunchOption> {
        self.options.iter().find(|o| o.id == id)
    }
}

/// What one mode narrows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeProfile {
    pub option_id: String,
    pub envelope: EnvelopeOverride,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    pub focus_id: String,
    /// In sort order.
    pub questions: Vec<LaunchQuestion>,
    pub profiles: Vec<ModeProfile>,
}

impl LaunchConfig {
    pub fn question_for_slot(&self, slot: &str) -> Option<&LaunchQuestion> {
        self.questions.iter().find(|q| q.slot == slot)
    }

    pub fn question(&self, id: &str) -> Option<&LaunchQuestion> {
        self.questions.iter().find(|q| q.id == id)
    }

    /// The question of kind `mode`, if the Focus declares one.
    pub fn mode_question(&self) -> Option<&LaunchQuestion> {
        self.questions.iter().find(|q| q.kind == QuestionKind::Mode)
    }

    /// The envelope override of a mode option, if it has a profile.
    pub fn mode_profile(&self, option_id: &str) -> Option<&EnvelopeOverride> {
        self.profiles
            .iter()
            .find(|p| p.option_id == option_id)
            .map(|p| &p.envelope)
    }

    /// Every rule of the shipped data. `registry` is the shipped document types: a
    /// document_type question may offer only shipped type ids (custom types are
    /// items.id=905's, which extends the submit command's allowed set, not this
    /// shipped data).
    pub fn validate(&self, registry: &Registry) -> Result<(), String> {
        let focus = &self.focus_id;
        let fail = |m: String| Err(format!("launch config of '{focus}': {m}"));

        if self.questions.len() > MAX_QUESTIONS {
            return fail(format!(
                "{} questions, and a Focus may ask at most {MAX_QUESTIONS}",
                self.questions.len()
            ));
        }
        let mut question_ids = BTreeSet::new();
        let mut slots = BTreeSet::new();
        for q in &self.questions {
            check_id(&format!("question id '{}'", q.id), &q.id)?;
            if !question_ids.insert(q.id.as_str()) {
                return fail(format!("question '{}' is listed twice", q.id));
            }
            if !SLOTS.contains(&q.slot.as_str()) {
                return fail(format!(
                    "question '{}' uses slot '{}', which is not one of {SLOTS:?}",
                    q.id, q.slot
                ));
            }
            if !slots.insert(q.slot.as_str()) {
                return fail(format!(
                    "two questions use the slot '{}' (one question per slot)",
                    q.slot
                ));
            }
            // The mode question owns launch_mode and nothing else may.
            if (q.kind == QuestionKind::Mode) != (q.slot == SLOT_LAUNCH_MODE) {
                return fail(format!(
                    "question '{}': only the mode question may use the slot \
                     '{SLOT_LAUNCH_MODE}', and the mode question must",
                    q.id
                ));
            }
            check_wording(
                &format!("question '{}' prompt", q.id),
                &q.prompt,
                PROMPT_MAX_BYTES,
                false,
            )?;
            if q.options.is_empty() {
                return fail(format!("question '{}' has no options", q.id));
            }
            if q.asked && q.options.len() < 2 {
                return fail(format!(
                    "question '{}' is asked but has one option (a fixed value is \
                     `asked` = 0)",
                    q.id
                ));
            }
            let mut option_ids = BTreeSet::new();
            for o in &q.options {
                check_id(&format!("option id '{}' of '{}'", o.id, q.id), &o.id)?;
                if !option_ids.insert(o.id.as_str()) {
                    return fail(format!("question '{}' lists option '{}' twice", q.id, o.id));
                }
                check_wording(
                    &format!("option '{}' of '{}' label", o.id, q.id),
                    &o.label,
                    LABEL_MAX_BYTES,
                    false,
                )?;
                check_wording(
                    &format!("option '{}' of '{}' hint", o.id, q.id),
                    &o.hint,
                    HINT_MAX_BYTES,
                    true,
                )?;
            }
            // Every question ships a default that is one of its options.
            if !option_ids.contains(q.default_option.as_str()) {
                return fail(format!(
                    "question '{}': default '{}' is not one of its options",
                    q.id, q.default_option
                ));
            }
            if q.slot == SLOT_DOCUMENT_TYPE {
                for o in &q.options {
                    if registry.document_type(&o.id).is_none() {
                        return fail(format!(
                            "question '{}': option '{}' is not a shipped document type",
                            q.id, o.id
                        ));
                    }
                }
            }
        }

        // Mode profiles: exactly one per option of the mode question, none elsewhere.
        let mode_options: BTreeSet<&str> = self
            .mode_question()
            .map(|q| q.options.iter().map(|o| o.id.as_str()).collect())
            .unwrap_or_default();
        let mut profiled = BTreeSet::new();
        for p in &self.profiles {
            if !mode_options.contains(p.option_id.as_str()) {
                return fail(format!(
                    "a mode profile names '{}', which is not an option of the mode question",
                    p.option_id
                ));
            }
            if !profiled.insert(p.option_id.as_str()) {
                return fail(format!("mode '{}' has two profiles", p.option_id));
            }
            check_envelope(&p.option_id, &p.envelope).map_err(|e| format!("{focus}: {e}"))?;
        }
        if let Some(missing) = mode_options.iter().find(|o| !profiled.contains(**o)) {
            return fail(format!("mode '{missing}' has no profile"));
        }
        Ok(())
    }
}

/// A mode profile can only name things it removes down to, never an empty set:
/// tighten() would refuse the empty result at run time, so the shipped data is
/// refused here first.
fn check_envelope(mode: &str, e: &EnvelopeOverride) -> Result<(), String> {
    if e.destinations.as_ref().is_some_and(Vec::is_empty) {
        return Err(format!("mode '{mode}': destinations is empty"));
    }
    if let Some(q) = e.quality_floor {
        if q.min > q.max {
            return Err(format!(
                "mode '{mode}': quality_floor.min is above quality_floor.max"
            ));
        }
    }
    if let Some(names) = &e.declared_inputs {
        for n in names {
            if !is_slug(n) {
                return Err(format!(
                    "mode '{mode}': declared input '{n}' must be a lower-case name"
                ));
            }
        }
    }
    Ok(())
}

fn check_id(what: &str, id: &str) -> Result<(), String> {
    if !is_slug(id) || id.len() > ID_MAX_CHARS {
        return Err(format!(
            "{what} must be a lower-case name of at most {ID_MAX_CHARS} characters"
        ));
    }
    Ok(())
}

/// Placeholder wording: no {token}, no brace, no NUL, within its byte cap. A hint
/// may be empty.
fn check_wording(
    what: &str,
    text: &str,
    max_bytes: usize,
    may_be_empty: bool,
) -> Result<(), String> {
    if text.is_empty() && may_be_empty {
        return Ok(());
    }
    if text.len() > max_bytes {
        return Err(format!("{what} is longer than {max_bytes} bytes"));
    }
    if text.contains('\0') {
        return Err(format!("{what} contains a NUL character"));
    }
    check_text(what, text, &[])
}

/// Check a one-line description (the display fact). The SQL CHECK covers length,
/// this covers the wording rules.
pub fn check_one_liner(focus_id: &str, text: &str) -> Result<(), String> {
    if text.chars().count() > ONE_LINER_MAX_CHARS {
        return Err(format!(
            "ui_one_liner of '{focus_id}' is longer than {ONE_LINER_MAX_CHARS} characters"
        ));
    }
    check_wording(
        &format!("ui_one_liner of '{focus_id}'"),
        text,
        4 * ONE_LINER_MAX_CHARS,
        false,
    )
}

// ---------------------------------------------------------------------------
// Reading shared.db
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LaunchConfigError {
    #[error("the launch tables could not be read: {0}")]
    Read(#[from] sqlx::Error),
    #[error("launch config of '{focus_id}': {message}")]
    Invalid { focus_id: String, message: String },
    /// validate() refused the rows. The message already names the Focus.
    #[error("{0}")]
    Validation(String),
}

fn invalid(focus_id: &str, message: impl Into<String>) -> LaunchConfigError {
    LaunchConfigError::Invalid {
        focus_id: focus_id.to_owned(),
        message: message.into(),
    }
}

/// Read a Focus's launch config from shared.db WITHOUT validating it. `None` when
/// the Focus declares no question.
///
/// PRIVATE ON PURPOSE (decisions.id=948, Chat-PM's stage 1 requirement): an
/// unvalidated config must never reach a consumer. The only way out of this module
/// is read_validated(), which runs validate() on the result, so LOAD, the admission
/// test and the submit command share one rule set and none can forget it. A source
/// scan below pins that nothing outside this file names this function.
async fn read_unvalidated(
    pool: &SqlitePool,
    focus_id: &str,
) -> Result<Option<LaunchConfig>, LaunchConfigError> {
    let question_rows = sqlx::query(
        "SELECT question_id, slot, kind, sort_order, asked, default_option, prompt
           FROM focus_launch_questions
          WHERE focus_id = ?
          ORDER BY sort_order, question_id",
    )
    .bind(focus_id)
    .fetch_all(pool)
    .await?;
    if question_rows.is_empty() {
        return Ok(None);
    }
    let option_rows = sqlx::query(
        "SELECT question_id, option_id, sort_order, label, hint
           FROM focus_launch_options
          WHERE focus_id = ?
          ORDER BY question_id, sort_order, option_id",
    )
    .bind(focus_id)
    .fetch_all(pool)
    .await?;
    let profile_rows = sqlx::query(
        "SELECT option_id, envelope
           FROM focus_mode_profiles
          WHERE focus_id = ?
          ORDER BY option_id",
    )
    .bind(focus_id)
    .fetch_all(pool)
    .await?;

    let mut questions = Vec::new();
    for r in &question_rows {
        let id: String = r.try_get("question_id")?;
        let kind_text: String = r.try_get("kind")?;
        let kind = QuestionKind::parse(&kind_text)
            .ok_or_else(|| invalid(focus_id, format!("question '{id}' has kind '{kind_text}'")))?;
        let asked: i64 = r.try_get("asked")?;
        let mut options = Vec::new();
        for o in &option_rows {
            let qid: String = o.try_get("question_id")?;
            if qid != id {
                continue;
            }
            options.push(LaunchOption {
                id: o.try_get("option_id")?,
                sort_order: o.try_get("sort_order")?,
                label: o.try_get("label")?,
                hint: o.try_get("hint")?,
            });
        }
        questions.push(LaunchQuestion {
            slot: r.try_get("slot")?,
            kind,
            sort_order: r.try_get("sort_order")?,
            asked: asked == 1,
            default_option: r.try_get("default_option")?,
            prompt: r.try_get("prompt")?,
            options,
            id,
        });
    }
    // An option row whose question is not declared is a data defect, not noise.
    for o in &option_rows {
        let qid: String = o.try_get("question_id")?;
        if !questions.iter().any(|q| q.id == qid) {
            return Err(invalid(
                focus_id,
                format!("an option belongs to the undeclared question '{qid}'"),
            ));
        }
    }
    let mut profiles = Vec::new();
    for p in &profile_rows {
        let option_id: String = p.try_get("option_id")?;
        let text: String = p.try_get("envelope")?;
        let envelope: EnvelopeOverride = serde_json::from_str(&text)
            .map_err(|e| invalid(focus_id, format!("the profile of mode '{option_id}': {e}")))?;
        profiles.push(ModeProfile {
            option_id,
            envelope,
        });
    }
    Ok(Some(LaunchConfig {
        focus_id: focus_id.to_owned(),
        questions,
        profiles,
    }))
}

// ---------------------------------------------------------------------------
// Submitting answers (items.id=897 stage 3, decisions.id=948 point 4)
// ---------------------------------------------------------------------------

/// Answers that passed validate_submission(): the only value the answer store
/// (persistence::launch_answer_store) accepts, so nothing stores an unvalidated
/// answer. The field is private and only validate_submission() builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAnswers {
    writes: Vec<(&'static str, String)>,
}

impl ValidatedAnswers {
    /// (bound name, value), in the order submitted.
    pub fn writes(&self) -> &[(&'static str, String)] {
        &self.writes
    }
}

/// Why a submit was refused. Each has a stable code for the command layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("the Focus asks no question '{0}'")]
    UnknownQuestion(String),
    #[error("question '{0}' is not asked (its value is fixed)")]
    QuestionNotAsked(String),
    #[error("the answer to question '{0}' is not one of its options")]
    InvalidAnswer(String),
    #[error("the per-run change is not a valid settings change: {0}")]
    InvalidRunChange(String),
}

impl SubmitError {
    /// The stable error code (stage 4 maps these to its command codes).
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownQuestion(_) => "unknown_question",
            Self::QuestionNotAsked(_) => "question_not_asked",
            Self::InvalidAnswer(_) => "invalid_answer",
            Self::InvalidRunChange(_) => "invalid_run_change",
        }
    }
}

/// Check a submit against the shipped data and the settings scales: every question
/// must be one the Focus asks, every answer one of its options (so no free text
/// reaches the store, and an unasked question cannot be set), and a per-run change
/// must parse as a SettingsPatch whose named steps exist. Fails closed; a question
/// named twice is refused. `config` is a validated config, so every option is a
/// well-formed id.
pub fn validate_submission(
    config: &LaunchConfig,
    answers: &[(&str, &str)],
    run_change: Option<&str>,
    registry: &Registry,
) -> Result<ValidatedAnswers, SubmitError> {
    use crate::conductor::bound_spec;
    use crate::conductor::model_block::document_type::{resolve_settings, SettingsPatch};

    let mut writes: Vec<(&'static str, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for (question_id, value) in answers {
        let q = config
            .question(question_id)
            .ok_or_else(|| SubmitError::UnknownQuestion((*question_id).to_owned()))?;
        if !seen.insert(q.id.as_str()) {
            return Err(SubmitError::InvalidAnswer(q.id.clone()));
        }
        if !q.asked {
            return Err(SubmitError::QuestionNotAsked(q.id.clone()));
        }
        if q.option(value).is_none() {
            return Err(SubmitError::InvalidAnswer(q.id.clone()));
        }
        let name = bound_spec::LAUNCH_SLOT_NAMES
            .iter()
            .copied()
            .find(|n| *n == q.slot)
            .expect("validate() admits only the compiled slots");
        writes.push((name, (*value).to_owned()));
    }
    if let Some(raw) = run_change {
        let shape_ok = bound_spec::spec(bound_spec::RUN_CHANGE).is_some_and(|s| (s.validate)(raw));
        if !shape_ok {
            return Err(SubmitError::InvalidRunChange(
                "it must be a small JSON object of the visible settings".to_owned(),
            ));
        }
        let patch: SettingsPatch =
            serde_json::from_str(raw).map_err(|e| SubmitError::InvalidRunChange(e.to_string()))?;
        // Scales are shared by every type, so any shipped type's defaults are a valid
        // base to check that each named step exists.
        let base = registry
            .types
            .first()
            .map(|t| t.settings.clone())
            .ok_or_else(|| SubmitError::InvalidRunChange("no document types".to_owned()))?;
        resolve_settings(&base, None, Some(&patch), &registry.scales)
            .map_err(SubmitError::InvalidRunChange)?;
        writes.push((bound_spec::RUN_CHANGE, raw.to_owned()));
    }
    Ok(ValidatedAnswers { writes })
}

/// The ids of every Focus that declares at least one launch question (shipped
/// data), for the admission test. Ids only: a config is read through
/// read_validated().
pub async fn declared_focus_ids(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT focus_id FROM focus_launch_questions ORDER BY focus_id")
        .fetch_all(pool)
        .await
}

/// Read a Focus's launch config and VALIDATE it: the one reader every consumer
/// uses (LOAD, the admission test, the stage 3 run-start hook and the submit
/// command). An invalid config is an error, never a partial result.
pub async fn read_validated(
    pool: &SqlitePool,
    focus_id: &str,
    registry: &Registry,
) -> Result<Option<LaunchConfig>, LaunchConfigError> {
    let Some(config) = read_unvalidated(pool, focus_id).await? else {
        return Ok(None);
    };
    config
        .validate(registry)
        .map_err(LaunchConfigError::Validation)?;
    Ok(Some(config))
}

/// LOAD lint (decisions.id=948 point 2): how a Focus's steps use the launch names,
/// against the Focus's own validated config (None when it declares no question).
///
///  * a step that names a slot (in its template, its declared inputs or a guard)
///    needs a question on that slot;
///  * a guard literal on a slot is one of that question's options (a typo in a
///    guard would otherwise make a row unreachable without a word);
///  * a step with a `requirement` (a model block) needs a document_type question,
///    because the model resolver's profile is read only from that seed: the name
///    must always have a value (rider of items.id=896 stage 3), and validate()
///    guarantees the question's default is one of its options.
///
/// `run_change` needs no question: it is the per-run settings patch and is absent
/// when there is no change. Question count, one question per slot and the defaults
/// are validate()'s.
pub fn lint(
    focus_id: &str,
    config: Option<&LaunchConfig>,
    steps: &[ActionDefinition],
) -> Result<(), String> {
    use crate::conductor::guard::Condition;

    let question_of = |slot: &str| config.and_then(|c| c.question_for_slot(slot));
    for step in steps {
        let mut named: BTreeSet<String> = step.declared_inputs.iter().cloned().collect();
        if step.reads_template() {
            named.extend(find_tokens(&step.prompt_template));
        }
        for g in step.guards() {
            named.insert(g.var.clone());
            let Some(slot) = SLOTS.iter().find(|s| **s == g.var) else {
                continue;
            };
            let Some(q) = question_of(slot) else {
                continue; // reported below as a missing question
            };
            let literals: Vec<&String> = match &g.condition {
                Condition::Eq(l) | Condition::Ne(l) => vec![l],
                Condition::In(v) => v.iter().collect(),
                Condition::Present | Condition::Absent => Vec::new(),
            };
            for l in literals {
                if q.option(l).is_none() {
                    return Err(format!(
                        "Focus '{focus_id}' step '{}': guard on '{slot}' compares with '{l}', \
                         which is not one of the options of its question '{}'",
                        step.action_id, q.id
                    ));
                }
            }
        }
        for slot in SLOTS {
            if named.contains(slot) && question_of(slot).is_none() {
                return Err(format!(
                    "Focus '{focus_id}' step '{}': names '{slot}', but the Focus declares no \
                     launch question on that slot",
                    step.action_id
                ));
            }
        }
        if step.requirement.is_some() && question_of(SLOT_DOCUMENT_TYPE).is_none() {
            return Err(format!(
                "Focus '{focus_id}' step '{}': a model block needs the run's document type, \
                 and the Focus declares no '{SLOT_DOCUMENT_TYPE}' question (a question with a \
                 default, asked or fixed): the resolver reads its profile only from that \
                 value, and without it the step would stop as too weak",
                step.action_id
            ));
        }
    }
    Ok(())
}

/// The optional one-line description of a Focus (the new display fact). An absent
/// row, or one that fails the wording rules, is None: it is display text only.
pub async fn read_one_liner(
    pool: &SqlitePool,
    focus_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT ui_one_liner FROM focus_launch_display WHERE focus_id = ?")
            .bind(focus_id)
            .fetch_optional(pool)
            .await?;
    Ok(text.filter(|t| check_one_liner(focus_id, t).is_ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::model_block::config::{Destination, QualityBar, QualityFloor};
    use crate::conductor::model_block::document_type::registry;
    use crate::conductor::tokens::ExternalAccess;

    fn opt(id: &str) -> LaunchOption {
        LaunchOption {
            id: id.to_owned(),
            sort_order: 0,
            label: format!("Label {id}"),
            hint: String::new(),
        }
    }

    fn question(id: &str, slot: &str, kind: QuestionKind, opts: &[&str]) -> LaunchQuestion {
        LaunchQuestion {
            id: id.to_owned(),
            slot: slot.to_owned(),
            kind,
            sort_order: 0,
            asked: true,
            default_option: opts[0].to_owned(),
            prompt: "Which one?".to_owned(),
            options: opts.iter().map(|o| opt(o)).collect(),
        }
    }

    fn profile(option: &str, envelope: EnvelopeOverride) -> ModeProfile {
        ModeProfile {
            option_id: option.to_owned(),
            envelope,
        }
    }

    /// A config with a mode question, a document type question and one choice.
    fn good() -> LaunchConfig {
        LaunchConfig {
            focus_id: "fx".to_owned(),
            questions: vec![
                question(
                    "how",
                    SLOT_LAUNCH_MODE,
                    QuestionKind::Mode,
                    &["quick", "careful"],
                ),
                question(
                    "kind",
                    SLOT_DOCUMENT_TYPE,
                    QuestionKind::Choice,
                    &["rma_letter", "cover_letter"],
                ),
                question(
                    "thorough",
                    "launch_answer_1",
                    QuestionKind::Choice,
                    &["simple", "polished"],
                ),
            ],
            profiles: vec![
                profile(
                    "quick",
                    EnvelopeOverride {
                        highest_tier: Some(ExternalAccess::LocalOnly),
                        destinations: Some(vec![Destination::Local]),
                        ..Default::default()
                    },
                ),
                profile(
                    "careful",
                    EnvelopeOverride {
                        quality_floor: Some(QualityFloor {
                            min: QualityBar::Standard,
                            max: QualityBar::High,
                        }),
                        ..Default::default()
                    },
                ),
            ],
        }
    }

    fn reg() -> &'static Registry {
        registry().expect("the bundled document types validate")
    }

    fn err(c: &LaunchConfig) -> String {
        c.validate(reg()).unwrap_err()
    }

    #[test]
    fn a_good_config_validates_and_a_focus_with_no_questions_is_valid() {
        good().validate(reg()).unwrap();
        LaunchConfig {
            focus_id: "none".to_owned(),
            questions: Vec::new(),
            profiles: Vec::new(),
        }
        .validate(reg())
        .unwrap();
    }

    #[test]
    fn more_than_five_questions_two_questions_on_one_slot_and_unknown_slots_are_refused() {
        let mut c = good();
        c.questions[2].slot = SLOT_DOCUMENT_TYPE.to_owned();
        assert!(
            err(&c).contains("two questions use the slot"),
            "{}",
            err(&c)
        );

        let mut c = good();
        c.questions[2].slot = "launch_answer_9".to_owned();
        assert!(err(&c).contains("not one of"), "{}", err(&c));

        let mut c = good();
        for i in 0..4 {
            let mut q = question(
                &format!("extra{i}"),
                "launch_answer_2",
                QuestionKind::Choice,
                &["a", "b"],
            );
            q.sort_order = 10 + i;
            c.questions.push(q);
        }
        assert!(err(&c).contains("at most 5"), "{}", err(&c));
    }

    #[test]
    fn the_mode_question_owns_launch_mode_and_nothing_else_may() {
        // A choice question on launch_mode (the mode question removed so the slot is
        // not also a duplicate).
        let mut c = good();
        c.questions.remove(0);
        c.profiles.clear();
        c.questions[1].slot = SLOT_LAUNCH_MODE.to_owned();
        assert!(err(&c).contains("only the mode question"), "{}", err(&c));
        let mut c = good();
        c.questions[0].kind = QuestionKind::Choice;
        assert!(err(&c).contains("only the mode question"), "{}", err(&c));
    }

    #[test]
    fn every_question_has_a_default_that_is_one_of_its_options() {
        let mut c = good();
        c.questions[1].default_option = "novel".to_owned();
        assert!(err(&c).contains("default 'novel'"), "{}", err(&c));
        let mut c = good();
        c.questions[1].options.clear();
        assert!(err(&c).contains("no options"), "{}", err(&c));
    }

    #[test]
    fn a_document_type_question_offers_only_shipped_types() {
        let mut c = good();
        c.questions[1].options.push(opt("my_own_kind"));
        assert!(
            err(&c).contains("not a shipped document type"),
            "{}",
            err(&c)
        );
    }

    #[test]
    fn a_fixed_question_may_have_one_option_an_asked_one_may_not() {
        let mut c = good();
        c.questions[1].options.truncate(1);
        assert!(err(&c).contains("asked but has one option"), "{}", err(&c));
        c.questions[1].asked = false;
        c.validate(reg()).unwrap();
    }

    #[test]
    fn every_mode_needs_exactly_one_profile_and_no_other_option_may_have_one() {
        let mut c = good();
        c.profiles.pop();
        assert!(err(&c).contains("has no profile"), "{}", err(&c));
        let mut c = good();
        c.profiles
            .push(profile("polished", EnvelopeOverride::default()));
        assert!(
            err(&c).contains("not an option of the mode question"),
            "{}",
            err(&c)
        );
        let mut c = good();
        c.profiles
            .push(profile("quick", EnvelopeOverride::default()));
        assert!(err(&c).contains("two profiles"), "{}", err(&c));
    }

    #[test]
    fn a_profile_that_would_empty_a_set_is_refused() {
        let mut c = good();
        c.profiles[0].envelope.destinations = Some(Vec::new());
        assert!(err(&c).contains("destinations is empty"), "{}", err(&c));
        let mut c = good();
        c.profiles[1].envelope.quality_floor = Some(QualityFloor {
            min: QualityBar::High,
            max: QualityBar::Basic,
        });
        assert!(
            err(&c).contains("quality_floor.min is above"),
            "{}",
            err(&c)
        );
    }

    #[test]
    fn wording_may_hold_no_token_brace_or_overlong_text() {
        let mut c = good();
        c.questions[0].prompt = "Pick {user_input}".to_owned();
        assert!(err(&c).contains("prompt"), "{}", err(&c));
        let mut c = good();
        c.questions[0].options[0].label = "x".repeat(LABEL_MAX_BYTES + 1);
        assert!(err(&c).contains("longer than"), "{}", err(&c));
        let mut c = good();
        c.questions[0].options[0].hint = "a hint".to_owned();
        c.validate(reg()).unwrap();
        c.questions[0].options[0].hint = "{x}".to_owned();
        assert!(err(&c).contains("hint"), "{}", err(&c));
        // Ids are lower-case names.
        let mut c = good();
        c.questions[2].options[0].id = "Not Valid".to_owned();
        assert!(err(&c).contains("option id"), "{}", err(&c));
    }

    #[test]
    fn the_one_liner_is_at_most_eighty_characters_and_plain() {
        check_one_liner("f", &"a".repeat(80)).unwrap();
        assert!(check_one_liner("f", &"a".repeat(81)).is_err());
        assert!(check_one_liner("f", "").is_err());
        assert!(check_one_liner("f", "say {user_input}").is_err());
    }

    // -- the shipped rows (needs the migrated shared database) -----------------

    use sqlx::SqlitePool;

    async fn migrated() -> (SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate");
        let pool = SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("pool");
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
        (pool, dir)
    }

    async fn insert(pool: &SqlitePool, sql: &str, binds: &[&str]) {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn only_the_writing_assistant_declares_questions() {
        // decisions.id=948 point 6: no real questions or modes shipped in items.id=897.
        // Writing Assistant's ship with its rebuild (items.id=903): two questions.
        let (pool, _dir) = migrated().await;
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM focus_launch_questions WHERE focus_id <> 'writing-assistant'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0);
        let wa: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM focus_launch_questions WHERE focus_id = 'writing-assistant'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(wa, 2);
        for f in ["quick-ask", "nothing"] {
            assert!(read_unvalidated(&pool, f).await.unwrap().is_none(), "{f}");
        }
    }

    #[tokio::test]
    async fn rows_round_trip_into_a_config_that_validates() {
        let (pool, _dir) = migrated().await;
        insert(
            &pool,
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('fx', 'how', 'launch_mode', 'mode', 1, 1, 'quick', 'How?')",
            &[],
        )
        .await;
        for (o, n) in [("quick", 1), ("careful", 2)] {
            sqlx::query(
                "INSERT INTO focus_launch_options
                   (focus_id, question_id, option_id, sort_order, label, hint)
                 VALUES ('fx', 'how', ?, ?, ?, '')",
            )
            .bind(o)
            .bind(n)
            .bind(format!("Label {o}"))
            .execute(&pool)
            .await
            .unwrap();
        }
        insert(
            &pool,
            "INSERT INTO focus_mode_profiles (focus_id, option_id, envelope)
             VALUES ('fx', 'quick', '{\"highest_tier\": \"local_only\"}'),
                    ('fx', 'careful', '{}')",
            &[],
        )
        .await;
        let c = read_unvalidated(&pool, "fx")
            .await
            .unwrap()
            .expect("a config");
        c.validate(reg()).unwrap();
        assert_eq!(c.questions.len(), 1);
        assert_eq!(c.questions[0].options.len(), 2);
        assert_eq!(
            c.mode_profile("quick").unwrap().highest_tier,
            Some(ExternalAccess::LocalOnly)
        );
        // Another Focus sees nothing.
        assert!(read_unvalidated(&pool, "fy").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unknown_kind_a_bad_envelope_and_an_orphan_option_fail_closed() {
        let (pool, _dir) = migrated().await;
        // The kind CHECK is in SQL, so an unknown kind cannot be stored, but a
        // malformed envelope and an orphan option can.
        insert(
            &pool,
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('fz', 'how', 'launch_mode', 'mode', 1, 1, 'quick', 'How?')",
            &[],
        )
        .await;
        insert(
            &pool,
            "INSERT INTO focus_mode_profiles (focus_id, option_id, envelope)
             VALUES ('fz', 'quick', '{\"gate1\": false}')",
            &[],
        )
        .await;
        let e = read_unvalidated(&pool, "fz").await.unwrap_err().to_string();
        assert!(e.contains("profile of mode 'quick'"), "{e}");
        sqlx::query("DELETE FROM focus_mode_profiles WHERE focus_id = 'fz'")
            .execute(&pool)
            .await
            .unwrap();
        insert(
            &pool,
            "INSERT INTO focus_launch_options
               (focus_id, question_id, option_id, sort_order, label)
             VALUES ('fz', 'ghost', 'a', 1, 'A')",
            &[],
        )
        .await;
        let e = read_unvalidated(&pool, "fz").await.unwrap_err().to_string();
        assert!(e.contains("undeclared question 'ghost'"), "{e}");
    }

    #[tokio::test]
    async fn the_sql_checks_refuse_a_bad_slot_a_second_mode_slot_and_a_long_one_liner() {
        let (pool, _dir) = migrated().await;
        let bad_slot = sqlx::query(
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('fx', 'q', 'launch_answer_9', 'choice', 1, 1, 'a', 'P')",
        )
        .execute(&pool)
        .await;
        assert!(bad_slot.is_err());
        let mode_off_slot = sqlx::query(
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('fx', 'q', 'document_type', 'mode', 1, 1, 'a', 'P')",
        )
        .execute(&pool)
        .await;
        assert!(mode_off_slot.is_err());
        let long = sqlx::query("INSERT INTO focus_launch_display VALUES ('fx', ?)")
            .bind("x".repeat(81))
            .execute(&pool)
            .await;
        assert!(long.is_err());
    }

    #[tokio::test]
    async fn the_three_shipped_one_liners_exist_and_pass_the_wording_rules() {
        let (pool, _dir) = migrated().await;
        for f in ["writing-assistant", "research-and-buy", "role-assessment"] {
            let t = read_one_liner(&pool, f).await.unwrap();
            assert!(t.is_some(), "{f} has a one-liner");
        }
        for f in ["quick-ask", "api-chat", "qr-admin"] {
            assert!(read_one_liner(&pool, f).await.unwrap().is_none(), "{f}");
        }
    }

    // -- the LOAD lint ---------------------------------------------------------

    use crate::conductor::guard::{Condition, Guard};
    use crate::conductor::model_block::config::{
        Difficulty, ModelRange, ModelRequirement, ModelTaskType, OutputSize, TaskNeed,
    };
    use crate::conductor::tokens::{ActionDefinition, StepType};
    use std::collections::HashMap;

    fn step(id: &str, template: &str) -> ActionDefinition {
        ActionDefinition {
            action_id: id.to_owned(),
            display_name: id.to_owned(),
            guide_id: "g".to_owned(),
            task_type: "general".to_owned(),
            routing_tier: ExternalAccess::LocalOnly,
            step_type: StepType::Generate,
            output_var: Some(format!("{id}_out")),
            prompt_template: template.to_owned(),
            field_requirements: HashMap::new(),
            options_override: HashMap::new(),
            external_access_override: None,
            requires_user_handoff: false,
            revisitable: false,
            block_stable_id: None,
            block_customization: None,
            schedule_trigger: None,
            guard: None,
            declared_inputs: Vec::new(),
            store_op: None,
            and_guards: Vec::new(),
            requirement: None,
        }
    }

    fn model_step(id: &str) -> ActionDefinition {
        let mut s = step(id, "write it");
        s.requirement = Some(ModelRequirement {
            block_stable_id: "cb-12".to_owned(),
            need: TaskNeed {
                task_type: ModelTaskType::CreativeWriting,
                difficulty: Difficulty::Low,
                output_size: OutputSize::Short,
            },
            range: ModelRange {
                quality_floor: QualityFloor {
                    min: QualityBar::Basic,
                    max: QualityBar::High,
                },
                highest_tier: ExternalAccess::LocalOnly,
                destinations: vec![Destination::Local],
            },
            class_preference: None,
        });
        s
    }

    fn guarded(mut s: ActionDefinition, var: &str, c: Condition) -> ActionDefinition {
        s.guard = Some(Guard {
            var: var.to_owned(),
            condition: c,
        });
        s
    }

    fn lint_err(c: Option<&LaunchConfig>, steps: &[ActionDefinition]) -> String {
        lint("fx", c, steps).unwrap_err()
    }

    #[test]
    fn a_step_naming_a_slot_needs_a_question_on_it() {
        let c = good();
        // document_type, launch_mode and launch_answer_1 have questions.
        lint(
            "fx",
            Some(&c),
            &[step("a", "{document_type} {launch_mode} {launch_answer_1}")],
        )
        .unwrap();
        // launch_answer_2 has none, in the template, a declaration or a guard.
        let e = lint_err(Some(&c), &[step("a", "{launch_answer_2}")]);
        assert!(
            e.contains("launch_answer_2") && e.contains("no launch question"),
            "{e}"
        );
        let mut declared = step("a", "x");
        declared.declared_inputs = vec!["launch_answer_2".to_owned()];
        assert!(lint_err(Some(&c), &[declared]).contains("launch_answer_2"));
        let g = guarded(step("a", "x"), "launch_answer_2", Condition::Present);
        assert!(lint_err(Some(&c), &[g]).contains("launch_answer_2"));
        // A Focus with no config at all.
        assert!(lint_err(None, &[step("a", "{launch_mode}")]).contains("launch_mode"));
        // A Focus that uses none is fine either way.
        lint("fx", None, &[step("a", "plain {user_input}")]).unwrap();
    }

    #[test]
    fn run_change_needs_no_question() {
        lint("fx", None, &[step("a", "{run_change}")]).unwrap();
        lint(
            "fx",
            None,
            &[guarded(step("a", "x"), "run_change", Condition::Present)],
        )
        .unwrap();
    }

    #[test]
    fn a_guard_literal_must_be_one_of_the_questions_options() {
        let c = good();
        for ok in [
            Condition::Eq("quick".to_owned()),
            Condition::Ne("careful".to_owned()),
            Condition::In(vec!["quick".to_owned(), "careful".to_owned()]),
            Condition::Present,
            Condition::Absent,
        ] {
            lint(
                "fx",
                Some(&c),
                &[guarded(step("a", "x"), "launch_mode", ok)],
            )
            .unwrap();
        }
        for bad in [
            Condition::Eq("quik".to_owned()),
            Condition::Ne("fast".to_owned()),
            Condition::In(vec!["quick".to_owned(), "slow".to_owned()]),
        ] {
            let e = lint_err(Some(&c), &[guarded(step("a", "x"), "launch_mode", bad)]);
            assert!(
                e.contains("not one of the options") && e.contains("'how'"),
                "{e}"
            );
        }
        // A guard on a name that is not a launch slot is not this lint's.
        lint(
            "fx",
            Some(&c),
            &[guarded(
                step("a", "x"),
                "locked_intent",
                Condition::Eq("anything".to_owned()),
            )],
        )
        .unwrap();
    }

    #[test]
    fn a_model_block_needs_a_document_type_question_asked_or_fixed() {
        let mut c = good();
        lint("fx", Some(&c), &[model_step("m")]).unwrap();
        // No config at all.
        let e = lint_err(None, &[model_step("m")]);
        assert!(e.contains("document_type") && e.contains("too weak"), "{e}");
        // A config with other questions only.
        c.questions.remove(1);
        let e = lint_err(Some(&c), &[model_step("m")]);
        assert!(e.contains("document_type"), "{e}");
        // A fixed question (a preset, asked = false) satisfies it.
        let mut fixed = good();
        fixed.questions[1].asked = false;
        fixed.questions[1].options.truncate(1);
        fixed.validate(reg()).unwrap();
        lint("fx", Some(&fixed), &[model_step("m")]).unwrap();
    }

    // -- the validated-only reader (Chat-PM's stage 1 requirement) -------------

    #[tokio::test]
    async fn read_validated_refuses_a_config_that_fails_validate() {
        let (pool, _dir) = migrated().await;
        // A default that is not one of the options: read_unvalidated returns it,
        // read_validated refuses it.
        insert(
            &pool,
            "INSERT INTO focus_launch_questions
               (focus_id, question_id, slot, kind, sort_order, asked, default_option, prompt)
             VALUES ('fbad', 'kind', 'document_type', 'choice', 1, 1, 'novel', 'Which?')",
            &[],
        )
        .await;
        for (o, n) in [("rma_letter", 1), ("cover_letter", 2)] {
            sqlx::query(
                "INSERT INTO focus_launch_options
                   (focus_id, question_id, option_id, sort_order, label, hint)
                 VALUES ('fbad', 'kind', ?, ?, 'L', '')",
            )
            .bind(o)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert!(read_unvalidated(&pool, "fbad").await.unwrap().is_some());
        let e = read_validated(&pool, "fbad", reg()).await.unwrap_err();
        assert!(matches!(e, LaunchConfigError::Validation(_)), "{e}");
        assert!(e.to_string().contains("default 'novel'"), "{e}");
        // Fix the default and it is returned.
        sqlx::query("UPDATE focus_launch_questions SET default_option = 'rma_letter'")
            .execute(&pool)
            .await
            .unwrap();
        let c = read_validated(&pool, "fbad", reg()).await.unwrap().unwrap();
        assert_eq!(c.questions[0].default_option, "rma_letter");
        // No rows: None.
        assert!(read_validated(&pool, "none-here", reg())
            .await
            .unwrap()
            .is_none());
    }

    fn rust_sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push((
                        p.to_string_lossy().into_owned(),
                        std::fs::read_to_string(&p).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out
    }

    const BANNED_READS: [&str; 6] = [
        "read_unvalidated",
        "launch_config::read(",
        "launch_config::read_raw",
        "FROM focus_launch_questions",
        "FROM focus_launch_options",
        "FROM focus_mode_profiles",
    ];

    /// Removes every "DELETE FROM focus_launch_questions|focus_launch_options|
    /// focus_mode_profiles" occurrence (items.id=898: the one Focus writer's delete).
    fn strip_launch_deletes(text: &str) -> String {
        let mut out = text.to_owned();
        for t in [
            "focus_launch_questions",
            "focus_launch_options",
            "focus_mode_profiles",
        ] {
            out = out.replace(&format!("DELETE FROM {t}"), "");
        }
        out
    }

    #[test]
    fn the_scan_allows_the_writers_deletes_and_still_fails_a_select() {
        let writer = "sqlx::query(\"DELETE FROM focus_launch_options WHERE focus_id = ?\");\n\
                      sqlx::query(\"DELETE FROM focus_mode_profiles WHERE focus_id = ?\");\n\
                      sqlx::query(\"DELETE FROM focus_launch_questions WHERE focus_id = ?\");";
        let s = strip_launch_deletes(writer);
        assert!(BANNED_READS.iter().all(|b| !s.contains(b)));
        let with_select = format!(
            "{writer}\nsqlx::query(\"SELECT option_id FROM focus_launch_options WHERE focus_id = ?\");"
        );
        let s = strip_launch_deletes(&with_select);
        assert!(BANNED_READS.iter().any(|b| s.contains(b)));
    }

    /// Chat-PM's stage 1 requirement: launch_config's reader does not validate, so
    /// every consumer must. The unvalidated reader is private to this file (the
    /// compiler enforces it), and this scan pins that nothing outside the file names
    /// it or an old public `read`, that the validated reader is what LOAD calls, and
    /// that every file that reads a config calls it (no file may build a LaunchConfig
    /// from rows itself).
    #[test]
    fn no_caller_can_use_an_unvalidated_config() {
        let this = "conductor/launch_config.rs";
        for (path, text) in rust_sources() {
            if path.ends_with(this) {
                continue;
            }
            for banned in BANNED_READS {
                // The admission test edits the tables to build fixture Focuses (it
                // writes rows, it builds no LaunchConfig from them), so the whole file
                // is allowed. The one Focus writer (items.id=898) is allowed ONLY its
                // DELETE statements: they are stripped and every banned string must
                // then be absent, so a SELECT in that file still fails.
                if path.ends_with("admission_tests.rs") {
                    continue;
                }
                let scanned = if path.ends_with("persistence/focus_composition_store.rs") {
                    strip_launch_deletes(&text)
                } else {
                    text.clone()
                };
                assert!(
                    !scanned.contains(banned),
                    "{path} names '{banned}': a launch config is read only through \
                     launch_config::read_validated"
                );
            }
        }
        let lifecycle = rust_sources()
            .into_iter()
            .find(|(p, _)| p.ends_with("conductor/lifecycle.rs"))
            .unwrap()
            .1;
        assert!(lifecycle.contains("launch_config::read_validated("));
        assert!(lifecycle.contains("launch_config::lint("));
        // The reader is `async fn read_unvalidated` (private): no `pub` in front. Lines
        // are matched from their start so this test's own text cannot satisfy it.
        let own = include_str!("launch_config.rs");
        assert!(own
            .lines()
            .any(|l| l.starts_with("async fn read_unvalidated(")));
        assert!(!own
            .lines()
            .any(|l| l.starts_with("pub async fn read_unvalidated(")
                || l.starts_with("pub(crate) async fn read_unvalidated(")
                || l.starts_with("pub async fn read(")));
    }

    // -- submitting answers (validate_submission) -------------------------------

    fn submit(
        answers: &[(&str, &str)],
        run_change: Option<&str>,
    ) -> Result<ValidatedAnswers, SubmitError> {
        validate_submission(&good(), answers, run_change, reg())
    }

    #[test]
    fn a_valid_submit_maps_each_question_to_its_slot_name_and_adds_the_run_change() {
        let v = submit(
            &[
                ("how", "careful"),
                ("kind", "cover_letter"),
                ("thorough", "polished"),
            ],
            Some(r#"{"tone":"firm"}"#),
        )
        .unwrap();
        assert_eq!(
            v.writes(),
            &[
                ("launch_mode", "careful".to_owned()),
                ("document_type", "cover_letter".to_owned()),
                ("launch_answer_1", "polished".to_owned()),
                ("run_change", r#"{"tone":"firm"}"#.to_owned()),
            ]
        );
        assert!(submit(&[], None).unwrap().writes().is_empty());
    }

    #[test]
    fn a_submit_refuses_an_unknown_unasked_duplicate_or_non_option_answer_with_its_own_code() {
        let e = submit(&[("nope", "quick")], None).unwrap_err();
        assert_eq!(e.code(), "unknown_question");
        let e = submit(&[("how", "slow")], None).unwrap_err();
        assert_eq!(e.code(), "invalid_answer");
        // Free text is never an option.
        let e = submit(&[("how", "quick; DROP TABLE")], None).unwrap_err();
        assert_eq!(e.code(), "invalid_answer");
        let e = submit(&[("how", "quick"), ("how", "careful")], None).unwrap_err();
        assert_eq!(e.code(), "invalid_answer");
        let mut c = good();
        c.questions[1].asked = false;
        c.questions[1].options.truncate(1);
        let e = validate_submission(&c, &[("kind", "rma_letter")], None, reg()).unwrap_err();
        assert_eq!(e.code(), "question_not_asked");
        // The error names the question, never the answer.
        assert!(!submit(&[("how", "slow")], None)
            .unwrap_err()
            .to_string()
            .contains("slow"));
    }

    #[test]
    fn a_run_change_must_be_a_settings_patch_whose_steps_exist() {
        for bad in [
            r#"{"highest_tier":"unrestricted"}"#,
            r#"{"destinations":["hosted_api"]}"#,
            r#"{"gate1":false}"#,
            "[]",
            "",
            r#"{"tone":"sparkly"}"#,
        ] {
            let e = submit(&[], Some(bad)).unwrap_err();
            assert_eq!(e.code(), "invalid_run_change", "{bad}");
        }
        submit(&[], Some("{}")).unwrap();
    }
}

// src-tauri/src/conductor/run_settings.rs
//
// items.id=897 stage 3 (decisions.id=942 point 8 and addendum 3 g, decisions.id=948):
// the RUN-START step. When a run is loaded, the user's launch answers are resolved
// once, and every model block of the Focus is re-expanded with them.
//
// WHAT IS RESOLVED (resolve(), pure):
//   * every question's value: the stored answer, or the question's default when
//     there is none, or always the default for a question that is not asked (a
//     preset). A stored answer that is not one of the question's options is a STALE
//     answer and fails the run closed (the shipped data may have changed since it
//     was given). Nothing is guessed and no default is used in its place.
//   * the mode (the value of the mode question) and the envelope override of its
//     profile.
//   * the settings, in three layers (decisions.id=942 addendum 3 g):
//       (1) the shipped default of the selected document type,
//       (2) the user's saved adjustments for that type (saved_settings_patch(), the
//           READ SEAM below, which returns nothing until items.id=905),
//       (3) run_change, the per-run patch.
//     resolve_settings() layers them field by field in either direction: settings
//     are the user's to move both ways. run_change is a SettingsPatch and parses
//     nothing else, so it cannot carry a tier, a destination, a declared input, a
//     quality floor or a gate (deny_unknown_fields, pinned).
//
// WHAT IS RE-EXPANDED (reexpand()): every ModelOrigin of the Focus, with the
// selected document type, the resolved settings and the mode's envelope override.
// EVERY ENVELOPE CHANGE GOES THROUGH tighten() (model_block::finish: the Focus
// ceiling clamp, then the mode profile), so a mode only removes. A mode profile
// applies to every model row of its Focus (decisions.id=948 point 6).
//
// WHAT NOTHING HERE DOES: approve a hosted or metered step (decisions.id=942 Q3, a
// source scan pins that spend and review code never names this module or a launch
// name), widen a ceiling, or switch a gate off. EXTERNAL_MODEL_BLOCKS_ALLOWED still
// refuses any external range at LOAD and in finish().
//
// THE LAYER-2 SEAM (for items.id=905). saved_settings_patch() is the ONE function
// that supplies the user's saved adjustments. In items.id=897 it returns Ok(None).
// items.id=905 replaces its body and provides: a per-persona ENCRYPTED store (never
// shared.db, which is unencrypted), validation of a stored patch through
// SettingsPatch and Scales::check_settings, custom document types fed into
// resolve_type and run_profile (executor.rs passes none today), the persona's custom
// ids in the submit command's allowed document_type set, and this function.
//
// THE TRIAL EXPANSION (trial_expand(), LOAD). Every model row is expanded against
// every document type option and every mode profile of its Focus, so a profile that
// empties the quality range or the destination list, removes a declared input, or
// otherwise cannot be applied refuses the Focus at LOAD and never stops a run.
//
// NOTHING IS STORED. The resolution lives on the FocusRun in memory only (as the
// step treatment does) and is re-derived at every load, including a resumed run.

use std::collections::BTreeMap;

use crate::conductor::bound_spec::{self, RUN_CHANGE};
use crate::conductor::launch_config::{
    LaunchConfig, LaunchQuestion, SLOT_DOCUMENT_TYPE, SLOT_LAUNCH_MODE,
};
use crate::conductor::lifecycle::FocusDefinition;
use crate::conductor::model_block::config::EnvelopeOverride;
use crate::conductor::model_block::document_type::{
    resolve_settings, Registry, Settings, SettingsPatch,
};
use crate::conductor::model_block::{
    model_block_spec, ExpandInput, ModelOrigin, EXTERNAL_MODEL_BLOCKS_ALLOWED,
};
use crate::conductor::tokens::ExternalAccess;

/// What a run resolved from its launch answers. Empty for a Focus with no launch
/// config. Seeded by seeding::seed_pass: the values of the Launch bound names come
/// from here and from nowhere else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchResolution {
    /// The value of each launch name this run has, by bound name: one per question
    /// (the slot's name) and run_change when a per-run change was stored.
    pub values: BTreeMap<&'static str, String>,
    /// The mode (the mode question's value), if the Focus has one.
    pub mode: Option<String>,
    /// The selected document type, if the Focus has the question.
    pub document_type: Option<String>,
    /// The settings in force: shipped default, saved adjustments, per-run change.
    pub settings: Option<Settings>,
}

/// Everything resolve() computes: the resolution, and the mode's envelope override
/// (which is not seeded, only applied through tighten()).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub resolution: LaunchResolution,
    pub mode_envelope: Option<EnvelopeOverride>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    /// A stored answer is not one of the question's options (stale or tampered).
    #[error("the stored answer to question '{question}' is not one of its options")]
    StaleAnswer { question: String },
    #[error("the stored per-run change is not a valid settings change: {0}")]
    InvalidRunChange(String),
    #[error("the selected document type is not a shipped document type")]
    UnknownDocumentType,
    #[error("the settings in force are not valid: {0}")]
    Settings(String),
    #[error("step '{step}' cannot be expanded for this run: {message}")]
    Expand { step: String, message: String },
}

/// What the run-start step reads from the chat's session store for one Focus: the
/// value of each launch name that has a stored row, by bound name.
pub type StoredAnswers = BTreeMap<String, String>;

/// Resolve a run's launch answers. Pure: no I/O.
pub fn resolve(
    config: Option<&LaunchConfig>,
    stored: &StoredAnswers,
    saved: Option<&SettingsPatch>,
    registry: &Registry,
) -> Result<Resolved, ResolveError> {
    let mut values: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut mode = None;
    let mut document_type = None;
    let mut mode_envelope = None;

    if let Some(config) = config {
        for q in &config.questions {
            let value = question_value(q, stored)?;
            let name = bound_spec::LAUNCH_SLOT_NAMES
                .iter()
                .copied()
                .find(|n| *n == q.slot)
                .expect("validate() admits only the compiled slots");
            if q.slot == SLOT_LAUNCH_MODE {
                mode_envelope = config.mode_profile(&value).cloned();
                mode = Some(value.clone());
            }
            if q.slot == SLOT_DOCUMENT_TYPE {
                document_type = Some(value.clone());
            }
            values.insert(name, value);
        }
    }

    // The per-run change, read whether or not the Focus has questions (a step may
    // name {run_change} on its own).
    let per_run = match stored.get(RUN_CHANGE) {
        None => None,
        Some(raw) => {
            let ok = bound_spec::spec(RUN_CHANGE).is_some_and(|s| (s.validate)(raw));
            if !ok {
                return Err(ResolveError::InvalidRunChange(
                    "it does not parse as a settings patch".to_owned(),
                ));
            }
            let patch: SettingsPatch = serde_json::from_str(raw)
                .map_err(|e| ResolveError::InvalidRunChange(e.to_string()))?;
            values.insert(RUN_CHANGE, raw.clone());
            Some(patch)
        }
    };

    let settings = match &document_type {
        None => None,
        Some(id) => {
            let t = registry
                .document_type(id)
                .ok_or(ResolveError::UnknownDocumentType)?;
            Some(
                resolve_settings(&t.settings, saved, per_run.as_ref(), &registry.scales)
                    .map_err(ResolveError::Settings)?,
            )
        }
    };

    Ok(Resolved {
        resolution: LaunchResolution {
            values,
            mode,
            document_type,
            settings,
        },
        mode_envelope,
    })
}

/// The value of one question: the stored answer when the question is asked and one
/// was stored (it must be an option), else the question's default. A question that
/// is not asked is a preset: its value is the default and a stored row is ignored.
fn question_value(q: &LaunchQuestion, stored: &StoredAnswers) -> Result<String, ResolveError> {
    if !q.asked {
        return Ok(q.default_option.clone());
    }
    match stored.get(q.slot.as_str()) {
        None => Ok(q.default_option.clone()),
        Some(v) if q.option(v).is_some() => Ok(v.clone()),
        Some(_) => Err(ResolveError::StaleAnswer {
            question: q.id.clone(),
        }),
    }
}

/// Re-expand every model block of `def` with a resolution. Replaces each expanded
/// step in place and keeps the row's guards and schedule trigger (LOAD set them
/// after the first expansion). Every envelope change goes through tighten() inside
/// the block's own finish().
pub fn reexpand(def: &mut FocusDefinition, resolved: &Resolved) -> Result<(), ResolveError> {
    let ceiling = def.max_routing_tier;
    let origins = def.model_origins.clone();
    for origin in &origins {
        let new = expand_origin(
            origin,
            ceiling,
            resolved.resolution.document_type.as_deref(),
            resolved.resolution.settings.as_ref(),
            resolved.mode_envelope.as_ref(),
        )
        .map_err(|message| ResolveError::Expand {
            step: origin.action_id.clone(),
            message,
        })?;
        let Some(slot) = def
            .steps
            .iter_mut()
            .find(|s| s.action_id == origin.action_id)
        else {
            return Err(ResolveError::Expand {
                step: origin.action_id.clone(),
                message: "the expanded step is missing from the Focus".to_owned(),
            });
        };
        let mut new = new;
        // items.id=903: the slot's list is already the MERGED one (the block's own
        // required-input clauses, then the row's), so it is kept whole. The fresh
        // expansion's clauses depend only on the spec's variable names, which a
        // re-expansion never changes; a mismatch would be a bug, so it is refused.
        if new
            .and_guards
            .iter()
            .any(|g| !slot.and_guards.contains(g) && slot.guard.as_ref() != Some(g))
        {
            return Err(ResolveError::Expand {
                step: origin.action_id.clone(),
                message: "the re-expanded block emits a required-input guard the loaded row \
                          does not carry"
                    .to_owned(),
            });
        }
        new.guard = slot.guard.take();
        new.and_guards = std::mem::take(&mut slot.and_guards);
        new.schedule_trigger = slot.schedule_trigger.take();
        *slot = new;
    }
    inject_check_settings(def, resolved)
}

/// items.id=903 (decisions.id=942 point 3): every cb-06 row with a draft check gets
/// the run's selected document type, latitude and pacing, from the SAME resolution
/// the model blocks were just re-expanded with, so the check and the draft agree. A
/// Focus with no document_type question (no selected type or settings) is left with
/// the row's own document type and its shipped defaults.
fn inject_check_settings(
    def: &mut FocusDefinition,
    resolved: &Resolved,
) -> Result<(), ResolveError> {
    let (Some(document_type), Some(settings)) = (
        resolved.resolution.document_type.as_ref(),
        resolved.resolution.settings.as_ref(),
    ) else {
        return Ok(());
    };
    for step in def.steps.iter_mut() {
        if step.block_stable_id.as_deref() != Some("cb-06") {
            continue;
        }
        let Some(custom) = step.block_customization.as_ref() else {
            continue;
        };
        let run = crate::conductor::check_block::RunCheckSettings {
            document_type: document_type.clone(),
            latitude: settings.latitude,
            pacing: settings.pacing.clone(),
        };
        let filled =
            crate::conductor::check_block::with_run_settings(custom, run).map_err(|message| {
                ResolveError::Expand {
                    step: step.action_id.clone(),
                    message,
                }
            })?;
        if let Some(filled) = filled {
            step.block_customization = Some(filled);
        }
    }
    Ok(())
}

fn expand_origin(
    origin: &ModelOrigin,
    ceiling: ExternalAccess,
    document_type: Option<&str>,
    settings: Option<&Settings>,
    envelope: Option<&EnvelopeOverride>,
) -> Result<crate::conductor::tokens::ActionDefinition, String> {
    let spec = model_block_spec(&origin.stable_id)
        .ok_or_else(|| format!("block '{}' is not a model block", origin.stable_id))?;
    (spec.expand)(
        &origin.parsed,
        &ExpandInput {
            action_id: &origin.action_id,
            focus_ceiling: ceiling,
            settings,
            external_allowed: EXTERNAL_MODEL_BLOCKS_ALLOWED,
            document_type,
            envelope_override: envelope,
        },
    )
}

/// LOAD: expand every model row against every document type option and every mode
/// profile of its Focus (the shipped default settings of each type), so a
/// combination that cannot be applied refuses the Focus here. A Focus with no model
/// row, or no launch config, has nothing to try.
pub fn trial_expand(
    config: Option<&LaunchConfig>,
    origins: &[ModelOrigin],
    ceiling: ExternalAccess,
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    if origins.is_empty() {
        return Ok(());
    }
    let types: Vec<Option<&str>> = match config.question_for_slot(SLOT_DOCUMENT_TYPE) {
        Some(q) => q.options.iter().map(|o| Some(o.id.as_str())).collect(),
        None => vec![None],
    };
    let modes: Vec<(Option<&str>, Option<&EnvelopeOverride>)> = match config.mode_question() {
        Some(q) => q
            .options
            .iter()
            .map(|o| (Some(o.id.as_str()), config.mode_profile(&o.id)))
            .collect(),
        None => vec![(None, None)],
    };
    for origin in origins {
        for t in &types {
            for (mode, envelope) in &modes {
                expand_origin(origin, ceiling, *t, None, *envelope).map_err(|e| {
                    format!(
                        "Focus '{}' step '{}' cannot be expanded for document type {} and mode {}: {e}",
                        config.focus_id,
                        origin.action_id,
                        t.map_or("(the row's own)".to_owned(), |t| format!("'{t}'")),
                        mode.map_or("(none)".to_owned(), |m| format!("'{m}'")),
                    )
                })?;
            }
        }
    }
    Ok(())
}

/// Who the saved adjustments are for.
pub struct SavedSettingsContext<'a> {
    pub user_id: &'a str,
    pub persona_id: &'a str,
    pub key_hex: &'a str,
}

/// THE LAYER-2 READ SEAM (items.id=905 replaces the body): the user's saved
/// adjustments for a document type, per persona. In items.id=897 nothing stores
/// them, so every run has no layer 2. See the module header for what 905 provides.
pub async fn saved_settings_patch(
    _ctx: &SavedSettingsContext<'_>,
    _document_type: &str,
) -> Result<Option<SettingsPatch>, String> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::launch_config::{LaunchOption, ModeProfile, QuestionKind};
    use crate::conductor::model_block::config::{Destination, QualityBar, QualityFloor};
    use crate::conductor::model_block::document_type::{registry, Latitude};

    fn reg() -> &'static Registry {
        registry().unwrap()
    }

    fn opt(id: &str) -> LaunchOption {
        LaunchOption {
            id: id.to_owned(),
            sort_order: 0,
            label: "L".to_owned(),
            hint: String::new(),
        }
    }

    fn q(id: &str, slot: &str, kind: QuestionKind, asked: bool, opts: &[&str]) -> LaunchQuestion {
        LaunchQuestion {
            id: id.to_owned(),
            slot: slot.to_owned(),
            kind,
            sort_order: 0,
            asked,
            default_option: opts[0].to_owned(),
            prompt: "P".to_owned(),
            options: opts.iter().map(|o| opt(o)).collect(),
        }
    }

    fn config() -> LaunchConfig {
        LaunchConfig {
            focus_id: "fx".to_owned(),
            questions: vec![
                q(
                    "how",
                    SLOT_LAUNCH_MODE,
                    QuestionKind::Mode,
                    true,
                    &["quick", "careful"],
                ),
                q(
                    "kind",
                    SLOT_DOCUMENT_TYPE,
                    QuestionKind::Choice,
                    true,
                    &["rma_letter", "cover_letter"],
                ),
                q(
                    "thorough",
                    "launch_answer_1",
                    QuestionKind::Choice,
                    true,
                    &["simple", "polished"],
                ),
            ],
            profiles: vec![
                ModeProfile {
                    option_id: "quick".to_owned(),
                    envelope: EnvelopeOverride {
                        highest_tier: Some(ExternalAccess::LocalOnly),
                        destinations: Some(vec![Destination::Local]),
                        ..Default::default()
                    },
                },
                ModeProfile {
                    option_id: "careful".to_owned(),
                    envelope: EnvelopeOverride {
                        quality_floor: Some(QualityFloor {
                            min: QualityBar::Standard,
                            max: QualityBar::High,
                        }),
                        ..Default::default()
                    },
                },
            ],
        }
    }

    fn stored(pairs: &[(&str, &str)]) -> StoredAnswers {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn nothing_stored_resolves_every_question_to_its_default() {
        let r = resolve(Some(&config()), &stored(&[]), None, reg()).unwrap();
        let v = &r.resolution.values;
        assert_eq!(v["launch_mode"], "quick");
        assert_eq!(v["document_type"], "rma_letter");
        assert_eq!(v["launch_answer_1"], "simple");
        assert!(!v.contains_key("run_change"));
        assert_eq!(r.resolution.mode.as_deref(), Some("quick"));
        assert_eq!(r.resolution.document_type.as_deref(), Some("rma_letter"));
        // The settings are the shipped default of the type.
        assert_eq!(
            r.resolution.settings.as_ref().unwrap(),
            &reg().document_type("rma_letter").unwrap().settings
        );
        assert_eq!(r.mode_envelope, config().mode_profile("quick").cloned());
    }

    #[test]
    fn a_stored_answer_wins_and_selects_the_mode_profile_and_the_type() {
        let r = resolve(
            Some(&config()),
            &stored(&[
                ("launch_mode", "careful"),
                ("document_type", "cover_letter"),
            ]),
            None,
            reg(),
        )
        .unwrap();
        assert_eq!(r.resolution.values["launch_mode"], "careful");
        assert_eq!(r.resolution.document_type.as_deref(), Some("cover_letter"));
        assert_eq!(r.mode_envelope, config().mode_profile("careful").cloned());
        assert_eq!(
            r.resolution.settings.as_ref().unwrap(),
            &reg().document_type("cover_letter").unwrap().settings
        );
    }

    #[test]
    fn a_stale_or_invalid_stored_answer_fails_the_run_and_is_never_replaced_by_a_default() {
        for (name, value) in [
            ("launch_mode", "gone"),
            ("document_type", "my_own_kind"),
            ("launch_answer_1", "Polished"),
            ("launch_answer_1", ""),
        ] {
            let e = resolve(Some(&config()), &stored(&[(name, value)]), None, reg()).unwrap_err();
            assert!(
                matches!(e, ResolveError::StaleAnswer { .. }),
                "{name}={value}: {e}"
            );
            // The error names the question, never the value.
            assert!(!e.to_string().contains(value) || value.is_empty());
        }
    }

    #[test]
    fn a_question_that_is_not_asked_is_a_preset_and_ignores_any_stored_row() {
        let mut c = config();
        c.questions[1].asked = false;
        c.questions[1].options.truncate(1);
        let r = resolve(
            Some(&c),
            &stored(&[("document_type", "cover_letter")]),
            None,
            reg(),
        )
        .unwrap();
        assert_eq!(r.resolution.document_type.as_deref(), Some("rma_letter"));
    }

    #[test]
    fn the_three_settings_layers_apply_in_order_and_either_direction() {
        // Layer 1: the shipped default of rma_letter is strict.
        let default = reg().document_type("rma_letter").unwrap().settings.clone();
        assert_eq!(default.latitude, Latitude::Strict);
        let saved = SettingsPatch {
            tone: Some("warm".to_owned()),
            latitude: Some(Latitude::Framed),
            ..Default::default()
        };
        // Layer 2 alone.
        let r = resolve(Some(&config()), &stored(&[]), Some(&saved), reg()).unwrap();
        let s = r.resolution.settings.unwrap();
        assert_eq!((s.tone.as_str(), s.latitude), ("warm", Latitude::Framed));
        assert_eq!(
            s.length, default.length,
            "an untouched field keeps the default"
        );
        // Layer 3 (run_change) beats layer 2 field by field, and may move a setting
        // back toward stricter as well as looser.
        let r = resolve(
            Some(&config()),
            &stored(&[("run_change", r#"{"tone":"firm","latitude":"strict"}"#)]),
            Some(&saved),
            reg(),
        )
        .unwrap();
        let s = r.resolution.settings.unwrap();
        assert_eq!((s.tone.as_str(), s.latitude), ("firm", Latitude::Strict));
        assert_eq!(
            r.resolution.values["run_change"],
            r#"{"tone":"firm","latitude":"strict"}"#
        );
    }

    #[test]
    fn a_run_change_that_is_not_a_settings_patch_or_names_an_unknown_step_fails_the_run() {
        for bad in [
            r#"{"highest_tier":"unrestricted"}"#,
            r#"{"destinations":["hosted_api"]}"#,
            r#"{"gate1":false}"#,
            "[]",
            "not json",
        ] {
            let e = resolve(
                Some(&config()),
                &stored(&[("run_change", bad)]),
                None,
                reg(),
            )
            .unwrap_err();
            assert!(matches!(e, ResolveError::InvalidRunChange(_)), "{bad}: {e}");
        }
        // A well-formed patch naming a step the scales do not have.
        let e = resolve(
            Some(&config()),
            &stored(&[("run_change", r#"{"tone":"sparkly"}"#)]),
            None,
            reg(),
        )
        .unwrap_err();
        assert!(matches!(e, ResolveError::Settings(_)), "{e}");
    }

    #[test]
    fn no_config_resolves_to_nothing_but_still_carries_a_run_change() {
        let r = resolve(None, &stored(&[]), None, reg()).unwrap();
        assert_eq!(r.resolution, LaunchResolution::default());
        assert_eq!(r.mode_envelope, None);
        let r = resolve(
            None,
            &stored(&[("run_change", r#"{"tone":"warm"}"#)]),
            None,
            reg(),
        )
        .unwrap();
        assert_eq!(r.resolution.values.len(), 1);
        assert!(
            r.resolution.settings.is_none(),
            "no type, no settings to resolve"
        );
    }

    #[tokio::test]
    async fn the_saved_settings_seam_returns_nothing_until_items_id_905() {
        let ctx = SavedSettingsContext {
            user_id: "u",
            persona_id: "p",
            key_hex: "k",
        };
        let got = saved_settings_patch(&ctx, "rma_letter").await;
        assert_eq!(got, Ok(None));
    }

    // -- a mode profile can only narrow (the property test) ---------------------

    fn origin() -> ModelOrigin {
        let parsed = crate::conductor::model_block::draft::parse(
            &crate::conductor::model_block::draft::example(),
            false,
        )
        .unwrap();
        ModelOrigin {
            action_id: "the_block".to_owned(),
            stable_id: "cb-12".to_owned(),
            parsed,
        }
    }

    /// Every envelope override over a small grid (and the empty one) either fails or
    /// yields a range inside the expansion without it, inside the Focus ceiling.
    /// A mode profile never adds a destination, raises a tier or lowers a floor.
    #[test]
    fn a_mode_profile_never_widens_the_expanded_range() {
        use crate::conductor::model_block::config::Destination as D;
        let tiers = [
            None,
            Some(ExternalAccess::LocalOnly),
            Some(ExternalAccess::AnonymousRequired),
            Some(ExternalAccess::Unrestricted),
        ];
        let dests: [Option<Vec<D>>; 8] = [
            None,
            Some(vec![D::Local]),
            Some(vec![D::HostedApi]),
            Some(vec![D::ManualHandoff]),
            Some(vec![D::Local, D::HostedApi]),
            Some(vec![D::HostedApi, D::ManualHandoff]),
            Some(vec![D::Local, D::HostedApi, D::ManualHandoff]),
            Some(vec![D::ManualHandoff, D::Local]),
        ];
        let bars = [QualityBar::Basic, QualityBar::Standard, QualityBar::High];
        let mut floors: Vec<Option<QualityFloor>> = vec![None];
        for min in bars {
            for max in bars {
                floors.push(Some(QualityFloor { min, max }));
            }
        }
        let o = origin();
        let mut ok_count = 0;
        for ceiling in [ExternalAccess::LocalOnly, ExternalAccess::AnonymousRequired] {
            let base = expand_origin(&o, ceiling, None, None, None).unwrap();
            let base_req = base.requirement.unwrap();
            for tier in &tiers {
                for d in &dests {
                    for f in &floors {
                        let over = EnvelopeOverride {
                            quality_floor: *f,
                            highest_tier: *tier,
                            destinations: d.clone(),
                            declared_inputs: None,
                        };
                        let Ok(step) = expand_origin(&o, ceiling, None, None, Some(&over)) else {
                            continue; // refused: nothing widened
                        };
                        ok_count += 1;
                        let r = step.requirement.unwrap().range;
                        assert!(r.highest_tier <= base_req.range.highest_tier, "{over:?}");
                        assert!(r.highest_tier <= ceiling, "{over:?}");
                        assert!(
                            r.quality_floor.min >= base_req.range.quality_floor.min,
                            "{over:?}"
                        );
                        assert!(
                            r.quality_floor.max <= base_req.range.quality_floor.max,
                            "{over:?}"
                        );
                        assert!(
                            r.destinations
                                .iter()
                                .all(|x| base_req.range.destinations.contains(x)),
                            "{over:?}"
                        );
                        // And the step stays inside what the Focus can reach.
                        if let Some(t) = step.external_access_override {
                            assert!(t <= ceiling);
                        }
                    }
                }
            }
        }
        assert!(ok_count > 0, "the grid must reach some success");
    }

    #[test]
    fn an_override_that_removes_a_declared_input_or_every_destination_is_refused() {
        let o = origin();
        let c = ExternalAccess::LocalOnly;
        let drop_input = EnvelopeOverride {
            declared_inputs: Some(vec!["content_brief".to_owned()]),
            ..Default::default()
        };
        let e = expand_origin(&o, c, None, None, Some(&drop_input)).unwrap_err();
        assert!(e.contains("remove a declared input"), "{e}");
        let none_left = EnvelopeOverride {
            destinations: Some(vec![Destination::HostedApi]),
            ..Default::default()
        };
        let e = expand_origin(&o, c, None, None, Some(&none_left)).unwrap_err();
        assert!(e.contains("no destination"), "{e}");
        // An identity override changes nothing.
        let base = expand_origin(&o, c, None, None, None).unwrap();
        let same = expand_origin(&o, c, None, None, Some(&EnvelopeOverride::default())).unwrap();
        assert_eq!(base.prompt_template, same.prompt_template);
        assert_eq!(base.requirement, same.requirement);
    }

    #[test]
    fn the_selected_type_and_settings_replace_the_rows_own() {
        let o = origin();
        let c = ExternalAccess::LocalOnly;
        let own = expand_origin(&o, c, None, None, None).unwrap();
        let other = expand_origin(&o, c, Some("creative_story"), None, None).unwrap();
        assert_ne!(own.prompt_template, other.prompt_template);
        let mut s = reg().document_type("rma_letter").unwrap().settings.clone();
        s.length = "long".to_owned();
        let long = expand_origin(&o, c, Some("rma_letter"), Some(&s), None).unwrap();
        assert_ne!(own.prompt_template, long.prompt_template);
        // An unknown type is an error, never a silent fall-back to the row's own.
        assert!(expand_origin(&o, c, Some("no_such_type"), None, None).is_err());
    }

    // -- nothing launch-related reaches the spend or review code (Q3) -------------

    /// decisions.id=942 Q3 and decisions.id=948 point 2: launching, or answering a
    /// launch question, never approves a hosted or metered step. A source scan pins
    /// that the spend gate, the plan and review code and the step treatment never
    /// name this module, the launch config, a launch name or the resolution.
    #[test]
    fn spend_review_and_treatment_code_never_name_a_launch_name() {
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
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root.join("spend"), &mut files);
        for f in [
            "conductor/step_review.rs",
            "conductor/step_treatment.rs",
            "conductor/provider_selection.rs",
            "persistence/step_review_store.rs",
            "persistence/provider_spend_store.rs",
            "commands/provider_spend.rs",
        ] {
            let p = root.join(f);
            files.push((
                f.to_owned(),
                std::fs::read_to_string(&p).unwrap_or_default(),
            ));
        }
        assert!(files.len() > 5, "the scan found the spend directory");
        for (path, text) in files {
            // Test modules may name anything: scan the code before them.
            let body = text.split("#[cfg(test)]").next().unwrap_or("");
            for banned in [
                "run_settings",
                "launch_config",
                "launch_answer",
                "LaunchResolution",
                "launch_mode",
                "run_change",
                "document_type",
            ] {
                assert!(!body.contains(banned), "{path} names '{banned}'");
            }
        }
        // The resolver is a selection module, not an approval: it names the document
        // type (its profile, the rider of items.id=896) and nothing else launch.
        let resolver = std::fs::read_to_string(root.join("conductor/model_resolver.rs")).unwrap();
        let resolver_body = resolver.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "run_settings",
            "launch_config",
            "launch_answer",
            "LaunchResolution",
            "launch_mode",
            "run_change",
        ] {
            assert!(!resolver_body.contains(banned), "resolver names '{banned}'");
        }
        // The resolver INPUT reads the document type seed (the rider) and nothing
        // else of the launch config, and the executor reads only that seed.
        let exec = std::fs::read_to_string(root.join("conductor/executor.rs")).unwrap();
        let exec_body = exec.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "launch_config",
            "launch_answer",
            "LaunchResolution",
            "run_settings",
            "launch_mode",
        ] {
            assert!(!exec_body.contains(banned), "executor names '{banned}'");
        }
    }
}

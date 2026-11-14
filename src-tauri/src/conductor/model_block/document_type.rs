// src-tauri/src/conductor/model_block/document_type.rs
//
// items.id=894 part 1 (decisions.id=942 addendum 3): document types and their
// settings, as DATA.
//
// Everything a block says to a model, and everything that differs between an RMA
// letter and a cover letter, lives in src-tauri/block_data/document_types.json
// (include_str!, parsed once, every entry validated by the same function a
// custom type will use): the latitude sentences, the facts-only and output-only
// lines, the section labels, the named steps of each voice setting and their
// plain phrases, and the shipped types. Rust holds only the template SKELETON
// (draft.rs, voice.rs, revise.rs): which phrase goes where and which declared
// variable goes in which slot. Tuning wording is a data edit, never a code
// change. The B7a document_types seed and focus_dev's emit-migration (B8) are
// generated FROM that file, so there is one source.
//
// A phrase may contain a slot written %name%: code fills it (a document type's
// form, a word count). A phrase may never contain a {token}: that syntax is the
// Conductor's, and a data edit must not be able to name {user_input} or
// {persona_context} (Prompt Authoring Rule, plus the LOAD hosted rule).
//
// The three layers of a document type's settings (addendum 3 g): the shipped
// default (DocumentType.settings), the user's saved adjustments and a per-run
// change. resolve_settings() layers them. Voice settings and latitude move
// freely in either direction. Only the four envelope fields of a block
// (highest tier, destinations, declared inputs, quality floor) are tighten-only,
// and they are not settings: a SettingsPatch has no field for them and
// deny_unknown_fields rejects a patch that names one.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::conductor::executor::find_tokens;
use crate::conductor::model_block::config::is_slug;

// ---------------------------------------------------------------------------
// Latitude and the check policy it implies
// ---------------------------------------------------------------------------

/// How much the model may add beyond the supplied points (addendum 3 d). A
/// document-type setting that replaces any fixed accuracy floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Latitude {
    /// Only supplied facts: an RMA letter, a complaint, a scholarly essay.
    Strict,
    /// Persuasive framing allowed, no new facts: a cover letter.
    Framed,
    /// Invention is the point: a creative story.
    Free,
}

impl Latitude {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Framed => "framed",
            Self::Free => "free",
        }
    }
}

/// What the unsupported-additions check does with a finding (numbers, names,
/// places and dates not supplied, brackets, opening chatter). The check itself
/// is a pure module ported at B6b; the regeneration is a guarded second draft
/// row in a composition (B7a, B7d), not block behaviour. Only the mapping lands
/// in items.id=894 (decisions.id=942 addendum 3 d).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPolicy {
    /// Regenerate once, then mark what is left.
    RegenerateOnceThenMark,
    /// Mark only.
    MarkOnly,
    /// Off.
    Off,
}

impl CheckPolicy {
    pub fn for_latitude(latitude: Latitude) -> Self {
        match latitude {
            Latitude::Strict => Self::RegenerateOnceThenMark,
            Latitude::Framed => Self::MarkOnly,
            Latitude::Free => Self::Off,
        }
    }
}

/// A sender detail a document type may receive (addendum 3 e). The block names
/// the declared-input variable that carries them; the type says which it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SenderField {
    Name,
    Address,
    Email,
    Phone,
    Date,
}

// ---------------------------------------------------------------------------
// Settings (the named steps a user can move) and the layers over them
// ---------------------------------------------------------------------------

/// A document type's settings: ids of named steps, never numbers (addendum 3 g).
/// Labels are Chat-BRAND's (items.id=506); these ids are internal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub directness: String,
    pub formality: String,
    pub pacing: String,
    pub tone: String,
    /// A step id of the length scale: short, standard or long.
    pub length: String,
    pub latitude: Latitude,
}

/// One layer over the settings (the user's saved adjustments, or a per-run
/// change). Every field optional; nothing else parses, so a patch cannot carry a
/// tier, a destination, a declared input or a quality floor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields, default)]
pub struct SettingsPatch {
    pub directness: Option<String>,
    pub formality: Option<String>,
    pub pacing: Option<String>,
    pub tone: Option<String>,
    pub length: Option<String>,
    pub latitude: Option<Latitude>,
}

/// The shipped default, then the user's saved adjustments, then a per-run
/// change: a later layer wins field by field, in either direction. Every named
/// step must exist in `scales`; nothing else is checked, because voice settings
/// and latitude are the user's to move both ways.
pub fn resolve_settings(
    default: &Settings,
    saved: Option<&SettingsPatch>,
    per_run: Option<&SettingsPatch>,
    scales: &Scales,
) -> Result<Settings, String> {
    let mut out = default.clone();
    for patch in [saved, per_run].into_iter().flatten() {
        if let Some(v) = &patch.directness {
            out.directness = v.clone();
        }
        if let Some(v) = &patch.formality {
            out.formality = v.clone();
        }
        if let Some(v) = &patch.pacing {
            out.pacing = v.clone();
        }
        if let Some(v) = &patch.tone {
            out.tone = v.clone();
        }
        if let Some(v) = &patch.length {
            out.length = v.clone();
        }
        if let Some(v) = patch.latitude {
            out.latitude = v;
        }
    }
    scales.check_settings(&out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Scales and phrases (data)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleStep {
    pub id: String,
    /// The plain sentence the voice block puts in the prompt for this step.
    pub phrase: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceScales {
    pub directness: Vec<ScaleStep>,
    pub formality: Vec<ScaleStep>,
    pub pacing: Vec<ScaleStep>,
    pub tone: Vec<ScaleStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scales {
    pub voice: VoiceScales,
    /// Step ids of the length setting, in order.
    pub length: Vec<String>,
}

impl Scales {
    fn step<'a>(steps: &'a [ScaleStep], id: &str) -> Option<&'a ScaleStep> {
        steps.iter().find(|s| s.id == id)
    }

    pub fn directness(&self, id: &str) -> Option<&ScaleStep> {
        Self::step(&self.voice.directness, id)
    }
    pub fn formality(&self, id: &str) -> Option<&ScaleStep> {
        Self::step(&self.voice.formality, id)
    }
    pub fn pacing(&self, id: &str) -> Option<&ScaleStep> {
        Self::step(&self.voice.pacing, id)
    }
    pub fn tone(&self, id: &str) -> Option<&ScaleStep> {
        Self::step(&self.voice.tone, id)
    }

    /// Every named step in `s` exists.
    pub fn check_settings(&self, s: &Settings) -> Result<(), String> {
        for (setting, id, found) in [
            (
                "directness",
                &s.directness,
                self.directness(&s.directness).is_some(),
            ),
            (
                "formality",
                &s.formality,
                self.formality(&s.formality).is_some(),
            ),
            ("pacing", &s.pacing, self.pacing(&s.pacing).is_some()),
            ("tone", &s.tone, self.tone(&s.tone).is_some()),
            ("length", &s.length, self.length.contains(&s.length)),
        ] {
            if !found {
                return Err(format!("settings: '{id}' is not a step of {setting}"));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        for (setting, steps) in [
            ("directness", &self.voice.directness),
            ("formality", &self.voice.formality),
            ("pacing", &self.voice.pacing),
            ("tone", &self.voice.tone),
        ] {
            if steps.len() < 2 {
                return Err(format!("scales: {setting} needs at least two steps"));
            }
            for (i, step) in steps.iter().enumerate() {
                if !is_slug(&step.id) {
                    return Err(format!(
                        "scales: {setting} step id '{}' is not a name",
                        step.id
                    ));
                }
                if steps[..i].iter().any(|s| s.id == step.id) {
                    return Err(format!(
                        "scales: {setting} step '{}' is listed twice",
                        step.id
                    ));
                }
                check_text(&format!("scales.{setting}.{}", step.id), &step.phrase, &[])?;
            }
        }
        if self.length.len() < 2 {
            return Err("scales: length needs at least two steps".to_owned());
        }
        for (i, id) in self.length.iter().enumerate() {
            if !is_slug(id) || self.length[..i].contains(id) {
                return Err(format!("scales: length step '{id}' is not a unique name"));
            }
        }
        Ok(())
    }
}

/// Every sentence the draft block emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftPhrases {
    /// Slot: %form%.
    pub lead: String,
    pub latitude_strict: String,
    pub latitude_framed: String,
    pub latitude_free: String,
    /// Slot: %form%.
    pub output_only: String,
    pub format_label: String,
    /// Slots: %min_words%, %max_words%.
    pub length: String,
    pub brief_label: String,
    pub sender_label: String,
    pub context_label: String,
}

/// Every sentence the voice block emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoicePhrases {
    pub apply_lead: String,
    pub apply_output_only: String,
    pub refine_lead: String,
    pub refine_output_only: String,
    pub draft_label: String,
    pub settings_label: String,
    pub directness_label: String,
    pub formality_label: String,
    pub pacing_label: String,
    pub tone_label: String,
    pub voice_profile_label: String,
}

/// Every sentence the revise block emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisePhrases {
    pub lead: String,
    pub latitude_strict: String,
    pub latitude_framed: String,
    pub latitude_free: String,
    pub output_only: String,
    pub draft_label: String,
    pub changes_label: String,
}

/// The labels a structured brief is rendered with (brief.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BriefPhrases {
    pub task_label: String,
    pub recipient_label: String,
    pub points_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phrases {
    pub draft: DraftPhrases,
    pub voice: VoicePhrases,
    pub revise: RevisePhrases,
    pub brief: BriefPhrases,
}

impl DraftPhrases {
    pub fn latitude(&self, l: Latitude) -> &str {
        match l {
            Latitude::Strict => &self.latitude_strict,
            Latitude::Framed => &self.latitude_framed,
            Latitude::Free => &self.latitude_free,
        }
    }
}

impl RevisePhrases {
    pub fn latitude(&self, l: Latitude) -> &str {
        match l {
            Latitude::Strict => &self.latitude_strict,
            Latitude::Framed => &self.latitude_framed,
            Latitude::Free => &self.latitude_free,
        }
    }
}

impl Phrases {
    fn validate(&self) -> Result<(), String> {
        let d = &self.draft;
        let v = &self.voice;
        let r = &self.revise;
        let b = &self.brief;
        let form: &[&str] = &["form"];
        let words: &[&str] = &["min_words", "max_words"];
        let none: &[&str] = &[];
        for (name, text, slots) in [
            ("draft.lead", &d.lead, form),
            ("draft.latitude_strict", &d.latitude_strict, none),
            ("draft.latitude_framed", &d.latitude_framed, none),
            ("draft.latitude_free", &d.latitude_free, none),
            ("draft.output_only", &d.output_only, form),
            ("draft.format_label", &d.format_label, none),
            ("draft.length", &d.length, words),
            ("draft.brief_label", &d.brief_label, none),
            ("draft.sender_label", &d.sender_label, none),
            ("draft.context_label", &d.context_label, none),
            ("voice.apply_lead", &v.apply_lead, none),
            ("voice.apply_output_only", &v.apply_output_only, none),
            ("voice.refine_lead", &v.refine_lead, none),
            ("voice.refine_output_only", &v.refine_output_only, none),
            ("voice.draft_label", &v.draft_label, none),
            ("voice.settings_label", &v.settings_label, none),
            ("voice.directness_label", &v.directness_label, none),
            ("voice.formality_label", &v.formality_label, none),
            ("voice.pacing_label", &v.pacing_label, none),
            ("voice.tone_label", &v.tone_label, none),
            ("voice.voice_profile_label", &v.voice_profile_label, none),
            ("revise.lead", &r.lead, none),
            ("revise.latitude_strict", &r.latitude_strict, none),
            ("revise.latitude_framed", &r.latitude_framed, none),
            ("revise.latitude_free", &r.latitude_free, none),
            ("revise.output_only", &r.output_only, none),
            ("revise.draft_label", &r.draft_label, none),
            ("revise.changes_label", &r.changes_label, none),
            ("brief.task_label", &b.task_label, none),
            ("brief.recipient_label", &b.recipient_label, none),
            ("brief.points_label", &b.points_label, none),
        ] {
            check_text(name, text, slots)?;
        }
        Ok(())
    }
}

/// Fill `%slot%` markers in a phrase. A phrase that was validated names only
/// slots in its allowed list, and the caller supplies each of them.
pub fn fill(phrase: &str, slots: &[(&str, &str)]) -> String {
    let mut out = phrase.to_owned();
    for (name, value) in slots {
        out = out.replace(&format!("%{name}%"), value);
    }
    out
}

/// A phrase or authored text is non-empty, holds no {token} (the Conductor's
/// syntax: a data edit must not name {user_input} or {persona_context}), and
/// names only the %slots% its position allows.
pub fn check_text(what: &str, text: &str, allowed_slots: &[&str]) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{what}: text is empty"));
    }
    if text.len() > 2_000 {
        return Err(format!("{what}: text is longer than 2000 bytes"));
    }
    if let Some(tok) = find_tokens(text).into_iter().next() {
        return Err(format!(
            "{what}: text names '{{{tok}}}', and authored text may contain no {{token}}"
        ));
    }
    if text.contains('{') || text.contains('}') {
        return Err(format!("{what}: text contains a brace"));
    }
    // %slot% markers.
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            return Err(format!("{what}: an unclosed % marker"));
        };
        let name = &after[..end];
        if !allowed_slots.contains(&name) {
            return Err(format!(
                "{what}: '%{name}%' is not a slot this text may use"
            ));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DocumentType
// ---------------------------------------------------------------------------

/// A word range and the generation limit that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WordRange {
    pub min_words: u32,
    pub max_words: u32,
    /// The model's token limit for this length (options_override.num_predict).
    pub num_predict: u32,
}

/// Drafting parameters. Author data, NOT a user setting.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StepParams {
    pub temperature: f32,
    pub top_p: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct TypeParams {
    pub draft: StepParams,
    pub voice: StepParams,
    pub revise: StepParams,
}

/// One document type. The shipped ones are rows of block_data/document_types.json
/// (shared by every Focus that uses the draft block). A custom type is the same
/// shape, stored per persona in the encrypted store (a later build) and validated
/// by the same function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct DocumentType {
    pub id: String,
    pub name: String,
    /// The word used in the prompt: "letter", "email", "essay section".
    pub form: String,
    /// Authored plain text about the format. No {token}.
    pub format_note: String,
    /// The shipped default of the user-facing settings.
    pub settings: Settings,
    /// A word range for every step of the length scale.
    pub length_words: BTreeMap<String, WordRange>,
    pub params: TypeParams,
    /// The sender details this type receives (addendum 3 e).
    pub sender: Vec<SenderField>,
    /// The personal fields this type may receive: a personal letter is not given
    /// the employer. Consumed by the document_context seed (B7a); until it
    /// exists a local draft receives none.
    pub context_fields: Vec<String>,
    /// A scholarly type flags references the user did not supply
    /// (decisions.id=548) whatever its latitude. Tighten-only against `based_on`.
    pub flag_unsupplied_references: bool,
    /// Custom types only: the shipped type this one was made from.
    pub based_on: Option<String>,
}

fn check_params(what: &str, p: &StepParams) -> Result<(), String> {
    if !p.temperature.is_finite() || !(0.0..=1.5).contains(&p.temperature) {
        return Err(format!("{what}: temperature must be between 0 and 1.5"));
    }
    if let Some(tp) = p.top_p {
        if !tp.is_finite() || !(0.0..=1.0).contains(&tp) || tp == 0.0 {
            return Err(format!("{what}: top_p must be above 0 and at most 1"));
        }
    }
    Ok(())
}

/// Validate a document type (shipped or custom). `base` is the shipped type a
/// custom type names in `based_on`: the unsupplied-references flag may not go
/// from true to false against it, so changing latitude cannot switch
/// decisions.id=548's flag off.
pub fn validate_document_type(
    t: &DocumentType,
    scales: &Scales,
    base: Option<&DocumentType>,
) -> Result<(), String> {
    let id = &t.id;
    if !is_slug(id) {
        return Err(format!(
            "document type '{id}': id must be a lower-case name"
        ));
    }
    check_text(&format!("document type '{id}'.name"), &t.name, &[])?;
    check_text(&format!("document type '{id}'.form"), &t.form, &[])?;
    check_text(
        &format!("document type '{id}'.format_note"),
        &t.format_note,
        &[],
    )?;
    scales
        .check_settings(&t.settings)
        .map_err(|e| format!("document type '{id}': {e}"))?;
    for step in &scales.length {
        let Some(r) = t.length_words.get(step) else {
            return Err(format!(
                "document type '{id}': length_words has no entry for the '{step}' step"
            ));
        };
        if r.min_words == 0 || r.min_words > r.max_words || r.max_words > 5_000 {
            return Err(format!(
                "document type '{id}': length '{step}' needs 0 < min_words <= max_words <= 5000"
            ));
        }
        if r.num_predict < r.max_words || r.num_predict > 8_192 {
            return Err(format!(
                "document type '{id}': length '{step}' needs max_words <= num_predict <= 8192"
            ));
        }
    }
    if let Some(extra) = t.length_words.keys().find(|k| !scales.length.contains(k)) {
        return Err(format!(
            "document type '{id}': length_words names '{extra}', which is not a length step"
        ));
    }
    check_params(
        &format!("document type '{id}'.params.draft"),
        &t.params.draft,
    )?;
    check_params(
        &format!("document type '{id}'.params.voice"),
        &t.params.voice,
    )?;
    check_params(
        &format!("document type '{id}'.params.revise"),
        &t.params.revise,
    )?;
    for (i, f) in t.sender.iter().enumerate() {
        if t.sender[..i].contains(f) {
            return Err(format!(
                "document type '{id}': sender field {f:?} is listed twice"
            ));
        }
    }
    for (i, f) in t.context_fields.iter().enumerate() {
        if !is_slug(f) {
            return Err(format!(
                "document type '{id}': context field '{f}' is not a name"
            ));
        }
        if t.context_fields[..i].contains(f) {
            return Err(format!(
                "document type '{id}': context field '{f}' is listed twice"
            ));
        }
    }
    match (&t.based_on, base) {
        (Some(b), Some(base)) => {
            if &base.id != b {
                return Err(format!(
                    "document type '{id}': based_on '{b}' does not match the base given"
                ));
            }
            if base.flag_unsupplied_references && !t.flag_unsupplied_references {
                return Err(format!(
                    "document type '{id}': flag_unsupplied_references cannot be switched off \
                     against its base '{b}' (decisions.id=548)"
                ));
            }
        }
        (Some(b), None) => {
            return Err(format!(
                "document type '{id}': based_on '{b}' was not given a base"
            ))
        }
        (None, Some(_)) => {
            return Err(format!(
                "document type '{id}': a base was given but based_on is null"
            ))
        }
        (None, None) => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The bundled registry
// ---------------------------------------------------------------------------

/// The shape of block_data/document_types.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub phrases: Phrases,
    pub scales: Scales,
    pub types: Vec<DocumentType>,
}

impl Registry {
    pub fn document_type(&self, id: &str) -> Option<&DocumentType> {
        self.types.iter().find(|t| t.id == id)
    }

    /// Everything the data file must satisfy. The shipped types are held to the
    /// same rules a custom type is.
    pub fn validate(&self) -> Result<(), String> {
        self.scales.validate()?;
        self.phrases.validate()?;
        if self.types.is_empty() {
            return Err("registry: no document types".to_owned());
        }
        for (i, t) in self.types.iter().enumerate() {
            if self.types[..i].iter().any(|o| o.id == t.id) {
                return Err(format!(
                    "registry: document type '{}' is listed twice",
                    t.id
                ));
            }
            if t.based_on.is_some() {
                return Err(format!(
                    "registry: shipped type '{}' must not carry based_on (only custom types do)",
                    t.id
                ));
            }
            validate_document_type(t, &self.scales, None)?;
        }
        Ok(())
    }
}

const BUNDLED: &str = include_str!("../../../block_data/document_types.json");

/// Parse and validate the bundled data once. An Err is a build defect (a test
/// parses the same file), and a block expansion reports it instead of running.
pub fn registry() -> Result<&'static Registry, String> {
    static REGISTRY: OnceLock<Result<Registry, String>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let reg: Registry = serde_json::from_str(BUNDLED)
                .map_err(|e| format!("block_data/document_types.json: {e}"))?;
            reg.validate()
                .map_err(|e| format!("block_data/document_types.json: {e}"))?;
            Ok(reg)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> &'static Registry {
        registry().expect("the bundled document types must validate")
    }

    fn rma() -> DocumentType {
        reg().document_type("rma_letter").unwrap().clone()
    }

    #[test]
    fn bundled_data_parses_and_every_type_validates() {
        let r = reg();
        for id in [
            "rma_letter",
            "cover_letter",
            "complaint_email",
            "personal_note",
            "essay_section",
            "creative_story",
        ] {
            assert!(r.document_type(id).is_some(), "{id} must ship");
        }
        for t in &r.types {
            validate_document_type(t, &r.scales, None).unwrap();
        }
    }

    #[test]
    fn shipped_latitudes_match_addendum_3() {
        let r = reg();
        for (id, l) in [
            ("rma_letter", Latitude::Strict),
            ("complaint_email", Latitude::Strict),
            ("essay_section", Latitude::Strict),
            ("cover_letter", Latitude::Framed),
            ("creative_story", Latitude::Free),
        ] {
            assert_eq!(r.document_type(id).unwrap().settings.latitude, l, "{id}");
        }
        // Scholarly types keep the reference flag whatever the latitude.
        assert!(
            r.document_type("essay_section")
                .unwrap()
                .flag_unsupplied_references
        );
        // A personal letter is not given the employer.
        assert!(!r
            .document_type("personal_note")
            .unwrap()
            .context_fields
            .iter()
            .any(|f| f == "employer"));
    }

    #[test]
    fn check_policy_follows_latitude() {
        assert_eq!(
            CheckPolicy::for_latitude(Latitude::Strict),
            CheckPolicy::RegenerateOnceThenMark
        );
        assert_eq!(
            CheckPolicy::for_latitude(Latitude::Framed),
            CheckPolicy::MarkOnly
        );
        assert_eq!(CheckPolicy::for_latitude(Latitude::Free), CheckPolicy::Off);
    }

    // -- resolve_settings: free both ways, nothing else ----------------------

    fn patch(f: impl FnOnce(&mut SettingsPatch)) -> SettingsPatch {
        let mut p = SettingsPatch::default();
        f(&mut p);
        p
    }

    #[test]
    fn voice_settings_move_both_directions_across_layers() {
        let r = reg();
        let d = rma().settings; // formality: formal
        let casual = patch(|p| p.formality = Some("casual".to_owned()));
        let formal = patch(|p| p.formality = Some("formal".to_owned()));
        // default -> saved loosens -> per-run tightens back.
        let s = resolve_settings(&d, Some(&casual), Some(&formal), &r.scales).unwrap();
        assert_eq!(s.formality, "formal");
        // default -> saved tightens (already formal) -> per-run loosens.
        let s = resolve_settings(&d, Some(&formal), Some(&casual), &r.scales).unwrap();
        assert_eq!(s.formality, "casual");
        // Every voice setting and the length can be set to every step, from every start.
        for setting in ["directness", "formality", "pacing", "tone"] {
            let steps: Vec<String> = match setting {
                "directness" => r
                    .scales
                    .voice
                    .directness
                    .iter()
                    .map(|s| s.id.clone())
                    .collect(),
                "formality" => r
                    .scales
                    .voice
                    .formality
                    .iter()
                    .map(|s| s.id.clone())
                    .collect(),
                "pacing" => r.scales.voice.pacing.iter().map(|s| s.id.clone()).collect(),
                _ => r.scales.voice.tone.iter().map(|s| s.id.clone()).collect(),
            };
            for from in &steps {
                for to in &steps {
                    let set = |id: &String| {
                        patch(|p| match setting {
                            "directness" => p.directness = Some(id.clone()),
                            "formality" => p.formality = Some(id.clone()),
                            "pacing" => p.pacing = Some(id.clone()),
                            _ => p.tone = Some(id.clone()),
                        })
                    };
                    let s =
                        resolve_settings(&d, Some(&set(from)), Some(&set(to)), &r.scales).unwrap();
                    let got = match setting {
                        "directness" => &s.directness,
                        "formality" => &s.formality,
                        "pacing" => &s.pacing,
                        _ => &s.tone,
                    };
                    assert_eq!(got, to);
                }
            }
        }
        for from in &r.scales.length {
            for to in &r.scales.length {
                let s = resolve_settings(
                    &d,
                    Some(&patch(|p| p.length = Some(from.clone()))),
                    Some(&patch(|p| p.length = Some(to.clone()))),
                    &r.scales,
                )
                .unwrap();
                assert_eq!(&s.length, to);
            }
        }
    }

    #[test]
    fn latitude_moves_both_directions_across_layers() {
        let r = reg();
        let d = rma().settings; // strict
        let lats = [Latitude::Strict, Latitude::Framed, Latitude::Free];
        for from in lats {
            for to in lats {
                let s = resolve_settings(
                    &d,
                    Some(&patch(|p| p.latitude = Some(from))),
                    Some(&patch(|p| p.latitude = Some(to))),
                    &r.scales,
                )
                .unwrap();
                assert_eq!(s.latitude, to);
            }
        }
        // strict -> free (looser) and free -> strict (tighter) both stand.
        let loose = resolve_settings(
            &d,
            None,
            Some(&patch(|p| p.latitude = Some(Latitude::Free))),
            &r.scales,
        )
        .unwrap();
        assert_eq!(loose.latitude, Latitude::Free);
        let back = resolve_settings(
            &loose,
            Some(&patch(|p| p.latitude = Some(Latitude::Strict))),
            None,
            &r.scales,
        )
        .unwrap();
        assert_eq!(back.latitude, Latitude::Strict);
    }

    #[test]
    fn a_later_layer_wins_and_an_unset_field_keeps_the_earlier_value() {
        let r = reg();
        let d = rma().settings;
        let saved = patch(|p| {
            p.tone = Some("warm".to_owned());
            p.pacing = Some("unhurried".to_owned());
        });
        let per_run = patch(|p| p.tone = Some("firm".to_owned()));
        let s = resolve_settings(&d, Some(&saved), Some(&per_run), &r.scales).unwrap();
        assert_eq!(s.tone, "firm");
        assert_eq!(s.pacing, "unhurried");
        assert_eq!(s.directness, d.directness);
        assert_eq!(resolve_settings(&d, None, None, &r.scales).unwrap(), d);
    }

    #[test]
    fn an_unknown_step_is_an_error() {
        let r = reg();
        let d = rma().settings;
        let bad = patch(|p| p.tone = Some("sarcastic".to_owned()));
        assert!(resolve_settings(&d, Some(&bad), None, &r.scales)
            .unwrap_err()
            .contains("not a step of tone"));
        let bad = patch(|p| p.length = Some("epic".to_owned()));
        assert!(resolve_settings(&d, None, Some(&bad), &r.scales).is_err());
    }

    #[test]
    fn a_patch_cannot_carry_an_envelope_field() {
        for key in [
            "highest_tier",
            "destinations",
            "declared_inputs",
            "quality_floor",
            "flag_unsupplied_references",
            "sender",
            "params",
            "temperature",
        ] {
            let json = format!(r#"{{"{key}": null}}"#);
            assert!(
                serde_json::from_str::<SettingsPatch>(&json).is_err(),
                "{key} must not parse as a settings patch"
            );
        }
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"latitude": "free"}"#).is_ok());
    }

    // -- validate_document_type ---------------------------------------------

    #[test]
    fn validation_rejects_each_fault() {
        let r = reg();
        let ok = rma();
        assert!(validate_document_type(&ok, &r.scales, None).is_ok());

        let mut t = ok.clone();
        t.id = "Bad Id".to_owned();
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.settings.tone = "sarcastic".to_owned();
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.length_words.remove("long");
        assert!(validate_document_type(&t, &r.scales, None)
            .unwrap_err()
            .contains("no entry"));

        let mut t = ok.clone();
        t.length_words.insert(
            "epic".to_owned(),
            WordRange {
                min_words: 1,
                max_words: 2,
                num_predict: 4,
            },
        );
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.length_words.get_mut("short").unwrap().num_predict = 1;
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.params.draft.temperature = 9.0;
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.sender = vec![SenderField::Name, SenderField::Name];
        assert!(validate_document_type(&t, &r.scales, None).is_err());

        let mut t = ok.clone();
        t.context_fields = vec!["Employer".to_owned()];
        assert!(validate_document_type(&t, &r.scales, None).is_err());
    }

    #[test]
    fn authored_text_may_contain_no_token_and_no_foreign_slot() {
        let r = reg();
        for bad in [
            "see {user_input}",
            "see {persona_context}",
            "any {thing_at_all}",
            "a stray { brace",
            "unclosed %form",
            "wrong %user% slot",
        ] {
            let mut t = rma();
            t.format_note = bad.to_owned();
            assert!(
                validate_document_type(&t, &r.scales, None).is_err(),
                "format_note '{bad}' must be refused"
            );
        }
        let mut t = rma();
        t.name = String::new();
        assert!(validate_document_type(&t, &r.scales, None).is_err());
    }

    #[test]
    fn a_custom_type_cannot_switch_the_reference_flag_off_against_its_base() {
        let r = reg();
        let base = r.document_type("essay_section").unwrap().clone();
        assert!(base.flag_unsupplied_references);

        let mut custom = base.clone();
        custom.id = "my_essay".to_owned();
        custom.based_on = Some("essay_section".to_owned());
        // Latitude is free to move (Strict -> Free) and the flag stays.
        custom.settings.latitude = Latitude::Free;
        assert!(validate_document_type(&custom, &r.scales, Some(&base)).is_ok());
        // The flag cannot go from true to false.
        custom.flag_unsupplied_references = false;
        assert!(validate_document_type(&custom, &r.scales, Some(&base))
            .unwrap_err()
            .contains("cannot be switched off"));
        // A base that does not carry the flag leaves a custom type free to choose.
        let plain = r.document_type("rma_letter").unwrap().clone();
        let mut custom = plain.clone();
        custom.id = "my_letter".to_owned();
        custom.based_on = Some("rma_letter".to_owned());
        custom.flag_unsupplied_references = true;
        assert!(validate_document_type(&custom, &r.scales, Some(&plain)).is_ok());
        custom.flag_unsupplied_references = false;
        assert!(validate_document_type(&custom, &r.scales, Some(&plain)).is_ok());
        // based_on and the base must agree.
        let mut custom = plain.clone();
        custom.id = "x".to_owned();
        assert!(validate_document_type(&custom, &r.scales, Some(&plain)).is_err());
        custom.based_on = Some("cover_letter".to_owned());
        assert!(validate_document_type(&custom, &r.scales, Some(&plain)).is_err());
        assert!(validate_document_type(&custom, &r.scales, None).is_err());
    }

    #[test]
    fn registry_validation_rejects_a_bad_phrase_a_duplicate_and_based_on() {
        let good = reg().clone();

        let mut r = good.clone();
        r.phrases.draft.latitude_strict = "Use {user_input} only.".to_owned();
        assert!(r.validate().unwrap_err().contains("{user_input}"));

        let mut r = good.clone();
        r.phrases.draft.lead = "Write a %form% in %tone%.".to_owned();
        assert!(r.validate().unwrap_err().contains("not a slot"));

        let mut r = good.clone();
        r.scales.voice.tone[0].phrase = "Be {persona_context}.".to_owned();
        assert!(r.validate().is_err());

        let mut r = good.clone();
        let dup = r.types[0].clone();
        r.types.push(dup);
        assert!(r.validate().unwrap_err().contains("listed twice"));

        let mut r = good.clone();
        r.types[0].based_on = Some("cover_letter".to_owned());
        assert!(r.validate().unwrap_err().contains("based_on"));
    }

    #[test]
    fn fill_replaces_named_slots() {
        assert_eq!(
            fill(
                "Aim for %min_words% to %max_words% words.",
                &[("min_words", "150"), ("max_words", "250")]
            ),
            "Aim for 150 to 250 words."
        );
    }
}

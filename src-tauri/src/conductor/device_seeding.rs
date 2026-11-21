// src-tauri/src/conductor/device_seeding.rs
//
// items.id=891 stage 6 (decisions.id=516, 509, 940): the two DERIVED bound names
// that put a Focus-owned device in front of a model, subject_context and
// device_library_brief (bound_spec.rs: Source::Derived, Presence::Always, text).
// Like case_context and the other derived names they are READ at pass start
// (seeding::seed_pass), never written, never a TaskStep, and never in the
// extraction input: every row that names one is tainted (conductor::taint). The text
// below is the LOCAL rendering. Since items.id=924 an external model step may name and
// declare either (a hand-off starter still may not:
// declared_inputs::DEVICE_NAMES_NEVER_IN_STARTER) and receives the EXTERNAL rendering
// of the pass's DeviceSeed instead (never facts dropped, a withheld name shown as
// "[device]", a device decisions.id=513 suppresses left out), whose ask values the
// carry scan raises in the outbound review.
//
// WHAT EACH IS
//   subject_context      the device the ACTIVE CASE is about, and what is recorded
//                        about it. The active case (session variable
//                        active_case_id) must be a case this Focus opened, its
//                        primary entity must be a device this Focus OWNS and that is
//                        ACTIVE; anything else is the empty string (a derived name
//                        always exists and an empty value says "nothing").
//   device_library_brief one line per active device the Focus owns, newest first,
//                        at most DEVICE_LINES. Each opens with the device's
//                        E-XXXXXXXX handle (persistence::handles), which a model
//                        names to pick an existing device (entity_find_or_create's
//                        handle slot). It serves subject identification (found,
//                        partial, not found: decisions.id=516 step 2) and the
//                        device-library intent. A device past the cap is still
//                        found by NAME, because entity_find_or_create matches by
//                        name when no handle is given.
//
// ONE PASS LATE. Seeds are read once, at the start of a pass (decisions.id=940
// addendum d, no mid-pass re-seed). The pass in which a device is first named or
// created therefore sees it in the brief at the earliest on the NEXT pass, and its
// facts in subject_context from the pass after the case is opened. That is the
// design, not a defect.
//
// WORDING AND CAPS (provisional, held for Chat-PM like items.id=914's texts). The
// Rust below holds only the LINE SKELETONS; every label comes from the device
// catalogue data (block_data/device_fields.json). Values and the name are cleaned
// like every derived value (seeding::clean: braces become parentheses so
// render_template() cannot read a stored "{x}" as a token).
//   subject_context   first line "Device: <name> (<class>)" (the class only when
//                     set), then one "<Label>: <value>" line per catalogued fact in
//                     CATALOGUE ORDER. A value is cut at SUBJECT_VALUE_MAX_BYTES
//                     on a character boundary. The text is capped at
//                     SUBJECT_CONTEXT_MAX_BYTES by DROPPING WHOLE LINES from the
//                     end, never cutting one in half.
//   device_library_brief   "- <handle>: <name> (<class>) | <Label>: <value> | ..."
//                     with up to HEADLINE_FACTS headline facts: the first facts in
//                     catalogue order that the catalogue marks pass at tier 2 (so a
//                     serial number or an address never appears in a list), each
//                     value cut at BRIEF_VALUE_MAX_BYTES. More than DEVICE_LINES
//                     devices end with "- and <n> more". The whole text is also
//                     bounded by bound_spec::DERIVED_TEXT_MAX_BYTES (seeding::cap).
//
// READS ONLY. Nothing here writes to either store.
//
// THE DEVICE SEED (items.id=924). Beside the two texts, a pass keeps in memory every
// catalogued value it seeded (and each seeded device's name) with the value's
// disclosure (device_fields::Disclosure): a DeviceSeed on the pass's StoreTokens, never
// persisted, serialized or logged (Debug prints counts only), rebuilt by every pass and
// so on resume. The carry scan of a hosted step (carry_scan::classify_devices) raises or
// removes every occurrence of an ask or never value of it. Its fingerprint (a SHA-256 of
// the entity ids, fields, values and disclosures, never a value) is what a checkpoint
// pins (resume_pin DEVICE_SEED_KEY): a device edited, renamed, added or re-disclosed
// between a park and its resume refuses the resume.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{
    self as spec, ACTIVE_CASE_ID, DEVICE_LIBRARY_BRIEF, SUBJECT_CONTEXT,
};
use crate::conductor::device_fields::{self, Catalogue, Disclosure, SUBJECT_CLASS_FIELD};
use crate::conductor::seeding::{cap, clean, SeedError};
use crate::conductor::visibility::{evaluate_object_visibility, SurfaceClass, VisibilityDecision};
use crate::persistence::case_store;
use crate::persistence::entity_store::Entity;
use crate::persistence::handles;
use crate::persistence::owned_entity_store::{self as owned, ActiveFact, OwnedEntityError};
use crate::persistence::session_store;

/// Most devices device_library_brief lists.
pub const DEVICE_LINES: usize = 8;
/// Most headline facts one brief line carries.
pub const HEADLINE_FACTS: usize = 3;
/// Longest value of a brief line, in bytes.
pub const BRIEF_VALUE_MAX_BYTES: usize = 60;
/// Longest value of a subject_context line, in bytes.
pub const SUBJECT_VALUE_MAX_BYTES: usize = 200;
/// Longest subject_context, in bytes (whole lines are dropped to fit).
pub const SUBJECT_CONTEXT_MAX_BYTES: usize = 1800;

/// items.id=924: the field name a device's NAME is scanned and logged under (it is not
/// a catalogue fact: entities.display_name).
pub const DEVICE_NAME_FIELD: &str = "device_name";

/// items.id=924: one value a pass seeded, with its disclosure. Debug never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct SeededValue {
    pub entity_id: String,
    /// A catalogue field name, or DEVICE_NAME_FIELD.
    pub field: String,
    /// The stored value (trimmed), before the brace cleaning of a rendering.
    pub value: String,
    pub disclosure: Disclosure,
    pub access_credential: bool,
    pub suggestion: Option<String>,
}

/// items.id=924 unit 3: what an EXTERNAL rendering of the device names shows for a
/// device whose name is never or that decisions.id=513 redacts at the Boundary.
pub const WITHHELD_DEVICE_NAME: &str = "[device]";

/// items.id=924 unit 3: one device as a pass read it, with every fact's EFFECTIVE
/// disclosure word (see effective()), for the external rendering.
#[derive(Clone, PartialEq, Eq)]
struct SeededDevice {
    device: BriefDevice,
    /// The effective disclosure of the name.
    name_disclosure: Disclosure,
    /// decisions.id=513's answer at the Boundary surface.
    visibility: VisibilityDecision,
}

/// items.id=924 unit 3: a value's disclosure once decisions.id=513 has spoken. A
/// device that is not Visible at the Boundary (redacted, suppressed, or anything else)
/// keeps its pass facts and turns its name and every other fact into never: Boundary
/// behaviour is invariant, so no answer in a review may release them.
fn effective(d: Disclosure, visibility: VisibilityDecision) -> Disclosure {
    if visibility == VisibilityDecision::Visible || d == Disclosure::Pass {
        d
    } else {
        Disclosure::Never
    }
}

/// items.id=924: what a pass seeded of the Focus's devices (see the header). Empty
/// when the pass named neither device name or the Focus has no device.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DeviceSeed {
    values: Vec<SeededValue>,
    /// How many active devices the brief counted (its "and n more"), when read.
    brief_total: Option<usize>,
    /// The active case's device, when subject_context was read and has one.
    subject: Option<SeededDevice>,
    /// The devices the brief listed, when it was read.
    brief: Vec<SeededDevice>,
}

impl std::fmt::Debug for DeviceSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSeed")
            .field("values", &self.values.len())
            .field("brief_total", &self.brief_total)
            .finish()
    }
}

impl DeviceSeed {
    pub fn values(&self) -> &[SeededValue] {
        &self.values
    }

    /// No device was seeded (a brief that found none included): no pin entry.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Add a device's name and catalogued facts, once per (entity, field), at their
    /// effective disclosure, and return it for the external rendering.
    fn add_device(
        &mut self,
        catalogue: &Catalogue,
        entity: &Entity,
        handle: String,
        facts: &[ActiveFact],
    ) -> SeededDevice {
        let visibility = evaluate_object_visibility(entity, "entity", SurfaceClass::Boundary, None);
        let entity_id = entity.id.as_str();
        let name_disclosure = effective(catalogue.device_name_disclosure, visibility);
        let mut push =
            |field: &str, value: &str, d: Disclosure, cred: bool, sugg: &Option<String>| {
                let value = value.trim();
                if value.is_empty()
                    || self
                        .values
                        .iter()
                        .any(|v| v.entity_id == entity_id && v.field == field)
                {
                    return;
                }
                self.values.push(SeededValue {
                    entity_id: entity_id.to_owned(),
                    field: field.to_owned(),
                    value: value.to_owned(),
                    disclosure: d,
                    access_credential: cred,
                    suggestion: sugg.clone(),
                });
            };
        push(
            DEVICE_NAME_FIELD,
            &entity.display_name,
            name_disclosure,
            false,
            &catalogue.device_name_suggestion,
        );
        for f in facts {
            if let Some(def) = catalogue.field(&f.field_name) {
                push(
                    &def.name,
                    &f.value,
                    effective(def.disclosure, visibility),
                    def.access_credential_flag,
                    &def.suggestion,
                );
            }
        }
        let (class, facts) = split_facts(catalogue, facts.to_vec());
        let facts = facts
            .into_iter()
            .map(|(label, value, word)| {
                // split_facts carries the catalogue's word; an unknown one is never.
                let d = match word.as_str() {
                    "pass" => Disclosure::Pass,
                    "ask" => Disclosure::Ask,
                    _ => Disclosure::Never,
                };
                (label, value, effective(d, visibility).as_str().to_owned())
            })
            .collect();
        SeededDevice {
            device: BriefDevice {
                handle,
                name: entity.display_name.clone(),
                class,
                facts,
            },
            name_disclosure,
            visibility,
        }
    }

    /// The name an external rendering shows: the device's own, or WITHHELD_DEVICE_NAME
    /// when its effective disclosure is never.
    fn external_device(d: &SeededDevice) -> BriefDevice {
        BriefDevice {
            name: if d.name_disclosure == Disclosure::Never {
                WITHHELD_DEVICE_NAME.to_owned()
            } else {
                d.device.name.clone()
            },
            facts: d
                .device
                .facts
                .iter()
                .filter(|(_, _, word)| word != Disclosure::Never.as_str())
                .cloned()
                .collect(),
            ..d.device.clone()
        }
    }

    fn shown_outside(d: &SeededDevice) -> bool {
        matches!(
            d.visibility,
            VisibilityDecision::Visible | VisibilityDecision::Redacted
        )
    }

    /// items.id=924 unit 3: subject_context as a HOSTED step receives it: the same
    /// skeleton, never facts dropped, a withheld name shown as WITHHELD_DEVICE_NAME, and
    /// nothing for a device decisions.id=513 suppresses (or when no device was seeded).
    pub fn external_subject_context(&self) -> String {
        match &self.subject {
            Some(d) if Self::shown_outside(d) => {
                let e = Self::external_device(d);
                let facts: Vec<(String, String)> =
                    e.facts.into_iter().map(|(l, v, _)| (l, v)).collect();
                render_subject_context(&e.name, e.class.as_deref(), &facts)
            }
            _ => String::new(),
        }
    }

    /// items.id=924 unit 3: device_library_brief as a HOSTED step receives it: a
    /// suppressed device left out, a withheld name shown as WITHHELD_DEVICE_NAME (the
    /// brief shows pass facts only, as locally).
    pub fn external_device_brief(&self) -> String {
        let shown: Vec<BriefDevice> = self
            .brief
            .iter()
            .filter(|d| Self::shown_outside(d))
            .map(Self::external_device)
            .collect();
        render_device_brief(&shown, self.brief_total.unwrap_or(shown.len()))
    }

    /// A seed of these values (tests outside this module).
    #[cfg(test)]
    pub(crate) fn for_test(values: Vec<SeededValue>, brief_total: Option<usize>) -> DeviceSeed {
        DeviceSeed {
            values,
            brief_total,
            ..Default::default()
        }
    }

    /// Tests outside this module: a device in memory, added as the active case's
    /// device (`as_subject`) or as a brief line, exactly as a pass adds one. Returns
    /// the LOCAL subject_context text a pass would seed for it.
    #[cfg(test)]
    pub(crate) fn add_for_test(
        &mut self,
        catalogue: &Catalogue,
        entity: &Entity,
        facts: &[ActiveFact],
        as_subject: bool,
    ) -> String {
        let seeded = self.add_device(catalogue, entity, format!("E-{}", entity.id), facts);
        if as_subject {
            self.subject = Some(seeded);
        } else {
            self.brief.push(seeded);
            self.brief_total = Some(self.brief.len());
        }
        let (class, f) = split_facts(catalogue, facts.to_vec());
        let f: Vec<(String, String)> = f.into_iter().map(|(l, v, _)| (l, v)).collect();
        render_subject_context(&entity.display_name, class.as_deref(), &f)
    }

    /// The pin of this seed: None when nothing was seeded (no entry is written), else
    /// a SHA-256 over every (entity, field, value, disclosure), sorted, and the brief's
    /// device count. Never a value.
    pub fn fingerprint(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut rows: Vec<String> = self
            .values
            .iter()
            .map(|v| {
                format!(
                    "{}\0{}\0{}\0{}",
                    v.entity_id,
                    v.field,
                    v.value,
                    v.disclosure.as_str()
                )
            })
            .collect();
        rows.sort();
        let mut h = Sha256::new();
        for r in &rows {
            h.update(r.as_bytes());
            h.update(b"\n");
        }
        h.update(format!("brief_total:{:?}", self.brief_total).as_bytes());
        Some(format!("{:x}", h.finalize()))
    }
}

/// items.id=924: the fact key of a device value in the review (remembered answers):
/// `device_fact:` and a SHA-256 of the entity, the field and the value, so a changed
/// value is asked again.
pub fn device_fact_key(entity_id: &str, field: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("{entity_id}\0{field}\0{}", value.trim()).as_bytes());
    format!("device_fact:{:x}", h.finalize())
}

/// True for the two names this module reads.
pub fn is_device_seed(name: &str) -> bool {
    name == SUBJECT_CONTEXT || name == DEVICE_LIBRARY_BRIEF
}

// ---------------------------------------------------------------------------
// Rendering (pure, so the exact text is testable)
// ---------------------------------------------------------------------------

/// One device as the brief shows it: its handle, name, class and every active
/// catalogued fact (label, value, disclosure word), in catalogue order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefDevice {
    pub handle: String,
    pub name: String,
    pub class: Option<String>,
    /// (label, value, disclosure word), catalogue order, subject_class excluded.
    pub facts: Vec<(String, String, String)>,
}

/// Cut at `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn name_of(raw: &str) -> String {
    let c = clean(raw);
    if c.is_empty() {
        "unnamed device".to_owned()
    } else {
        c
    }
}

fn with_class(name: &str, class: Option<&str>) -> String {
    match class.map(clean).filter(|c| !c.is_empty()) {
        Some(c) => format!("{name} ({c})"),
        None => name.to_owned(),
    }
}

/// subject_context for one device: `facts` are (label, value) in catalogue order,
/// the subject class excluded.
pub fn render_subject_context(
    name: &str,
    class: Option<&str>,
    facts: &[(String, String)],
) -> String {
    let mut lines = vec![format!("Device: {}", with_class(&name_of(name), class))];
    lines.extend(
        facts
            .iter()
            .map(|(label, value)| (clean(label), clean(value)))
            .filter(|(_, v)| !v.is_empty())
            .map(|(label, value)| format!("{label}: {}", cut(&value, SUBJECT_VALUE_MAX_BYTES))),
    );
    // Whole lines are dropped from the end to fit; the first line always stays
    // (cut on a character boundary only if it alone is over the cap).
    let mut out = String::new();
    for line in lines {
        let extra = if out.is_empty() { 0 } else { 1 } + line.len();
        if !out.is_empty() && out.len() + extra > SUBJECT_CONTEXT_MAX_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    cut(&out, SUBJECT_CONTEXT_MAX_BYTES)
}

/// device_library_brief for `devices` (newest first, already in order). `total` is
/// how many active devices the Focus owns, which may exceed the devices passed.
pub fn render_device_brief(devices: &[BriefDevice], total: usize) -> String {
    let mut lines: Vec<String> = devices
        .iter()
        .take(DEVICE_LINES)
        .map(|d| {
            let mut line = format!(
                "- {}: {}",
                d.handle,
                with_class(&name_of(&d.name), d.class.as_deref())
            );
            for (label, value, _) in d
                .facts
                .iter()
                .filter(|(_, _, disclosure)| disclosure == "pass")
                .take(HEADLINE_FACTS)
            {
                let value = clean(value);
                if value.is_empty() {
                    continue;
                }
                line.push_str(&format!(
                    " | {}: {}",
                    clean(label),
                    cut(&value, BRIEF_VALUE_MAX_BYTES)
                ));
            }
            line
        })
        .collect();
    if total > DEVICE_LINES {
        lines.push(format!("- and {} more", total - DEVICE_LINES));
    }
    cap(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn store_err(name: &'static str, e: impl std::fmt::Display) -> SeedError {
    SeedError::Store {
        name,
        message: e.to_string(),
    }
}

/// The catalogued facts of a device as (label, value, disclosure word) in catalogue
/// order, the subject class apart.
fn split_facts(
    catalogue: &Catalogue,
    facts: Vec<ActiveFact>,
) -> (Option<String>, Vec<(String, String, String)>) {
    let mut class = None;
    let mut rest = Vec::new();
    for f in facts {
        if f.field_name == SUBJECT_CLASS_FIELD {
            class = Some(f.value);
            continue;
        }
        if let Some(def) = catalogue.field(&f.field_name) {
            rest.push((
                def.label.clone(),
                f.value,
                def.disclosure.as_str().to_owned(),
            ));
        }
    }
    (class, rest)
}

/// Read the device seeds named in `names` (a subset of subject_context and
/// device_library_brief), and the DeviceSeed of what they hold (items.id=924). Every
/// value must satisfy its own spec, and any store read error fails the pass closed,
/// like every other derived name.
pub async fn read_device_seeds(
    outputs: &mut SqliteConnection,
    personal: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    names: &[&'static str],
) -> Result<(HashMap<String, String>, DeviceSeed), SeedError> {
    let catalogue = device_fields::registry().map_err(|e| store_err(SUBJECT_CONTEXT, e))?;
    let mut out: HashMap<String, String> = HashMap::new();
    let mut seed = DeviceSeed::default();

    if names.contains(&SUBJECT_CONTEXT) {
        out.insert(
            SUBJECT_CONTEXT.to_owned(),
            subject_context(
                outputs,
                personal,
                catalogue,
                context_key,
                focus_id,
                &mut seed,
            )
            .await?,
        );
    }

    if names.contains(&DEVICE_LIBRARY_BRIEF) {
        let devices = owned::list_owned_active(personal, focus_id)
            .await
            .map_err(|e| store_err(DEVICE_LIBRARY_BRIEF, e))?;
        let mut shown = Vec::new();
        for d in devices.iter().take(DEVICE_LINES) {
            let facts = owned::list_active_facts(personal, focus_id, &d.id)
                .await
                .map_err(|e| store_err(DEVICE_LIBRARY_BRIEF, e))?;
            let seeded = seed.add_device(
                catalogue,
                d,
                handles::entity_handle(focus_id, &d.id),
                &facts,
            );
            seed.brief.push(seeded);
            let (class, facts) = split_facts(catalogue, facts);
            shown.push(BriefDevice {
                handle: handles::entity_handle(focus_id, &d.id),
                name: d.display_name.clone(),
                class,
                facts,
            });
        }
        seed.brief_total = Some(devices.len());
        out.insert(
            DEVICE_LIBRARY_BRIEF.to_owned(),
            render_device_brief(&shown, devices.len()),
        );
    }

    // A value that fails its spec fails the pass closed rather than reaching a
    // prompt.
    for name in names {
        let s = spec::spec(name).expect("device seeds are registry names");
        match out.get(*name) {
            Some(v) if (s.validate)(v) => {}
            _ => return Err(SeedError::InvalidValue(name)),
        }
    }
    Ok((out, seed))
}

async fn subject_context(
    outputs: &mut SqliteConnection,
    personal: &mut SqliteConnection,
    catalogue: &Catalogue,
    context_key: Option<&str>,
    focus_id: &str,
    seed: &mut DeviceSeed,
) -> Result<String, SeedError> {
    // No chat means no active case.
    let Some(key) = context_key else {
        return Ok(String::new());
    };
    let active = session_store::get_session_variable(outputs, key, focus_id, ACTIVE_CASE_ID)
        .await
        .map_err(|e| store_err(SUBJECT_CONTEXT, e))?;
    let Some(active) = active else {
        return Ok(String::new());
    };
    let valid = spec::spec(ACTIVE_CASE_ID).is_some_and(|s| (s.validate)(&active.value));
    if !valid {
        return Err(SeedError::InvalidValue(SUBJECT_CONTEXT));
    }
    // Only a case this Focus opened is its case.
    let case = case_store::get_case(outputs, &active.value)
        .await
        .map_err(|e| store_err(SUBJECT_CONTEXT, e))?
        .filter(|c| c.focus_id == focus_id);
    let Some(entity_id) = case.and_then(|c| c.primary_entity_id) else {
        return Ok(String::new());
    };
    // The device must be this Focus's own and active (a purged, archived or
    // another Focus's entity reads as nothing).
    let entity = match owned::get_owned_active(personal, focus_id, &entity_id).await {
        Ok(e) => e,
        Err(OwnedEntityError::NotFound) => return Ok(String::new()),
        Err(e) => return Err(store_err(SUBJECT_CONTEXT, e)),
    };
    let facts = owned::list_active_facts(personal, focus_id, &entity_id)
        .await
        .map_err(|e| store_err(SUBJECT_CONTEXT, e))?;
    seed.subject = Some(seed.add_device(catalogue, &entity, String::new(), &facts));
    let (class, facts) = split_facts(catalogue, facts);
    let facts: Vec<(String, String)> = facts.into_iter().map(|(l, v, _)| (l, v)).collect();
    Ok(render_subject_context(
        &entity.display_name,
        class.as_deref(),
        &facts,
    ))
}

/// Tests: an entity in memory with decisions.id=513's two flags.
#[cfg(test)]
pub(crate) fn entity_for_test(id: &str, name: &str, redact: bool, hide: bool) -> Entity {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "entity_type": "subject",
        "display_name": name,
        "aliases": [],
        "parent_entity_id": null,
        "status": "active",
        "modification_state": "user_created",
        "source_registry_id": null,
        "source_url": null,
        "created_at": "t",
        "extra_metadata": {},
        "redact_identification": redact,
        "hide_from_shared_surfaces": hide,
    }))
    .expect("an entity")
}

/// Tests: the RMA fixture device (synthetic values): a laptop with two pass facts, the
/// serial and the purchase date (ask), and a warranty contact.
#[cfg(test)]
pub(crate) fn rma_facts_for_test() -> Vec<ActiveFact> {
    [
        ("subject_class", "device:endpoint:laptop"),
        ("make", "Lenovo"),
        ("model", "ThinkPad X1"),
        ("purchase_date", "2025-03-14"),
        ("serial_number", "PF2ABC3D"),
        ("warranty_contact", "help@warranty.example"),
    ]
    .iter()
    .map(|(f, v)| ActiveFact {
        field_name: (*f).to_owned(),
        value: (*v).to_owned(),
        created_at: "t".to_owned(),
    })
    .collect()
}

/// Tests: the shipped catalogue with `field` made never (no shipped field is never).
#[cfg(test)]
pub(crate) fn catalogue_with_never_for_test(field: &str) -> Catalogue {
    let mut c = device_fields::registry().expect("catalogue").clone();
    let f = c
        .fields
        .iter_mut()
        .find(|f| f.name == field)
        .expect("a field");
    f.disclosure = Disclosure::Never;
    f.suggestion = None;
    c.validate().expect("still valid");
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::run_settings::LaunchResolution;
    use crate::persistence::case_store::{open_case, NewCase};
    use crate::persistence::output_store::open_outputs_db;
    use crate::persistence::personal_store::open_personal_db;
    use crate::persistence::session_store::{put_session_variable, SessionWrite};

    const USER: &str = "u-dseed";
    const PERSONA: &str = "p-dseed";
    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const CHAT: &str = "chat-dseed";
    const FOCUS: &str = "tech-support";
    const OTHER: &str = "other-focus";

    fn fact(label: &str, value: &str, tier2: &str) -> (String, String, String) {
        (label.to_owned(), value.to_owned(), tier2.to_owned())
    }

    // -- the exact rendered text -------------------------------------------------

    /// THE WORDING, pinned for review. A device with a typical set of facts.
    #[test]
    fn subject_context_text_for_one_device_with_a_typical_set_of_facts() {
        let facts = vec![
            ("Make".to_owned(), "Netgear".to_owned()),
            ("Model".to_owned(), "R7000".to_owned()),
            ("Firmware version".to_owned(), "1.0.4.82".to_owned()),
            ("Connection".to_owned(), "Ethernet to the modem".to_owned()),
            ("Serial number".to_owned(), "4XF1234567890".to_owned()),
            (
                "Known issues".to_owned(),
                "drops the connection at night".to_owned(),
            ),
        ];
        assert_eq!(
            render_subject_context("Living room router", Some("device:network:router"), &facts),
            "Device: Living room router (device:network:router)\n\
             Make: Netgear\n\
             Model: R7000\n\
             Firmware version: 1.0.4.82\n\
             Connection: Ethernet to the modem\n\
             Serial number: 4XF1234567890\n\
             Known issues: drops the connection at night"
        );
        // No class, no facts: the device line alone.
        assert_eq!(
            render_subject_context("Spare tablet", None, &[]),
            "Device: Spare tablet"
        );
    }

    /// THE WORDING, pinned for review. Three devices: one with no facts, one partial,
    /// one full, newest first.
    #[test]
    fn device_library_brief_text_for_three_devices() {
        let devices = vec![
            BriefDevice {
                handle: "E-7K3M9XQA".to_owned(),
                name: "Spare tablet".to_owned(),
                class: None,
                facts: vec![],
            },
            BriefDevice {
                handle: "E-2B8VNC4D".to_owned(),
                name: "Study laptop".to_owned(),
                class: Some("device:endpoint:laptop".to_owned()),
                facts: vec![fact("Operating system", "Windows 11", "pass")],
            },
            BriefDevice {
                handle: "E-QH5T0WRE".to_owned(),
                name: "Living room router".to_owned(),
                class: Some("device:network:router".to_owned()),
                facts: vec![
                    fact("Make", "Netgear", "pass"),
                    fact("Model", "R7000", "pass"),
                    fact("Firmware version", "1.0.4.82", "pass"),
                    fact("Last updated", "2026-09-30", "pass"),
                    fact("Serial number", "4XF1234567890", "not_permitted"),
                ],
            },
        ];
        assert_eq!(
            render_device_brief(&devices, 3),
            "- E-7K3M9XQA: Spare tablet\n\
             - E-2B8VNC4D: Study laptop (device:endpoint:laptop) | Operating system: Windows 11\n\
             - E-QH5T0WRE: Living room router (device:network:router) | Make: Netgear | Model: R7000 | Firmware version: 1.0.4.82"
        );
        // Nothing owned: empty, which says "nothing".
        assert_eq!(render_device_brief(&[], 0), "");
    }

    #[test]
    fn the_brief_shows_only_facts_the_catalogue_lets_pass_and_at_most_three() {
        let d = BriefDevice {
            handle: "E-AAAAAAAA".to_owned(),
            name: "Router".to_owned(),
            class: None,
            facts: vec![
                fact("Serial number", "SN-SECRET", "not_permitted"),
                fact("Retailer", "Best Buy", "omit"),
                fact("Make", "Netgear", "pass"),
                fact("Model", "R7000", "pass"),
                fact("Firmware version", "1", "pass"),
                fact("Last updated", "today", "pass"),
            ],
        };
        let text = render_device_brief(&[d], 1);
        assert_eq!(
            text,
            "- E-AAAAAAAA: Router | Make: Netgear | Model: R7000 | Firmware version: 1"
        );
        assert!(!text.contains("SN-SECRET") && !text.contains("Best Buy"));
    }

    #[test]
    fn more_than_eight_devices_end_with_a_count_and_never_list_a_ninth() {
        let devices: Vec<BriefDevice> = (0..12)
            .map(|i| BriefDevice {
                handle: format!("E-{i:08}"),
                name: format!("Device {i}"),
                class: None,
                facts: vec![],
            })
            .collect();
        let text = render_device_brief(&devices, 12);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), DEVICE_LINES + 1);
        assert_eq!(lines[7], "- E-00000007: Device 7");
        assert_eq!(lines[8], "- and 4 more");
    }

    // -- caps ----------------------------------------------------------------------

    #[test]
    fn the_caps_are_these_and_cut_on_character_boundaries() {
        assert_eq!(
            (
                DEVICE_LINES,
                HEADLINE_FACTS,
                BRIEF_VALUE_MAX_BYTES,
                SUBJECT_VALUE_MAX_BYTES,
                SUBJECT_CONTEXT_MAX_BYTES
            ),
            (8, 3, 60, 200, 1800)
        );
        const { assert!(SUBJECT_CONTEXT_MAX_BYTES <= spec::DERIVED_TEXT_MAX_BYTES) };
        // A value over the cap is cut at the cap (2-byte characters: never mid-char).
        let long = "é".repeat(150); // 300 bytes
        let text = render_subject_context("D", None, &[("Make".to_owned(), long.clone())]);
        let value = text.lines().nth(1).unwrap().strip_prefix("Make: ").unwrap();
        assert_eq!(value.len(), SUBJECT_VALUE_MAX_BYTES);
        assert!(value.chars().all(|c| c == 'é'));
        let brief = render_device_brief(
            &[BriefDevice {
                handle: "E-AAAAAAAA".to_owned(),
                name: "D".to_owned(),
                class: None,
                facts: vec![fact("Make", &long, "pass")],
            }],
            1,
        );
        assert_eq!(
            brief.rsplit_once("Make: ").unwrap().1.len(),
            BRIEF_VALUE_MAX_BYTES
        );
    }

    #[test]
    fn subject_context_drops_whole_lines_to_fit_and_never_cuts_one_in_half() {
        let facts: Vec<(String, String)> = (0..40)
            .map(|i| (format!("Field {i:02}"), "v".repeat(150)))
            .collect();
        let text = render_subject_context("Router", Some("device:network:router"), &facts);
        assert!(text.len() <= SUBJECT_CONTEXT_MAX_BYTES, "{}", text.len());
        for line in text.lines().skip(1) {
            assert!(
                line.ends_with(&"v".repeat(150)),
                "a line was cut in half: {line}"
            );
        }
        let lines = text.lines().count();
        assert!(lines > 5 && lines < 41, "{lines}");
        // The first line always stays, even if it alone is over the cap.
        let huge = render_subject_context(&"n".repeat(5000), None, &[]);
        assert_eq!(huge.len(), SUBJECT_CONTEXT_MAX_BYTES);
    }

    #[test]
    fn braces_and_nuls_are_neutralised_in_every_part() {
        let text = render_subject_context(
            "R{user_input}",
            Some("device:x"),
            &[("Make".to_owned(), "{persona_context}\0".to_owned())],
        );
        assert!(
            !text.contains('{') && !text.contains('}') && !text.contains('\0'),
            "{text}"
        );
        let brief = render_device_brief(
            &[BriefDevice {
                handle: "E-AAAAAAAA".to_owned(),
                name: "{x}".to_owned(),
                class: Some("{c}".to_owned()),
                facts: vec![fact("Make", "{m}", "pass")],
            }],
            1,
        );
        assert!(!brief.contains('{') && !brief.contains('}'), "{brief}");
        // An empty name reads as an unnamed device, never a blank line.
        assert_eq!(
            render_subject_context("  ", None, &[]),
            "Device: unnamed device"
        );
    }

    // -- reading real files --------------------------------------------------------

    struct TestRoot {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
        _dir: tempfile::TempDir,
    }

    impl TestRoot {
        async fn new() -> Self {
            let lock = crate::test_support::ENV_MUTEX.lock().await;
            let saved = std::env::var("QR_DATA_ROOT").ok();
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("QR_DATA_ROOT", dir.path());
            TestRoot {
                _lock: lock,
                saved,
                _dir: dir,
            }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    struct Db {
        outputs: SqliteConnection,
        personal: SqliteConnection,
    }

    async fn dbs() -> Db {
        Db {
            outputs: open_outputs_db(USER, PERSONA, KEY).await.unwrap(),
            personal: open_personal_db(USER, PERSONA, KEY).await.unwrap(),
        }
    }

    async fn device(db: &mut Db, focus: &str, name: &str, key: &str) -> String {
        owned::create_subject(&mut db.personal, focus, PERSONA, name, None, key)
            .await
            .unwrap()
    }

    async fn put(db: &mut Db, id: &str, field: &str, value: &str) {
        owned::put_owned_fact(&mut db.personal, FOCUS, PERSONA, id, field, value)
            .await
            .unwrap();
    }

    async fn activate(db: &mut Db, case_id: &str, focus: &str, entity: &str) {
        open_case(
            &mut db.outputs,
            &NewCase {
                case_id,
                focus_id: focus,
                context_key: CHAT,
                primary_entity_id: entity,
                subject_label: "label",
                symptoms: "s",
                opened_by_run: "run-1",
            },
        )
        .await
        .unwrap();
        put_session_variable(
            &mut db.outputs,
            &SessionWrite {
                context_key: CHAT,
                focus_id: focus,
                name: ACTIVE_CASE_ID,
                value: case_id,
                written_by_run: "run-1",
                expected_version: None,
            },
        )
        .await
        .unwrap();
    }

    async fn read(
        db: &mut Db,
        key: Option<&str>,
        names: &[&'static str],
    ) -> HashMap<String, String> {
        read_device_seeds(&mut db.outputs, &mut db.personal, key, FOCUS, names)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn the_subject_is_the_active_cases_device_and_never_another_focus_or_purged() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let router = device(&mut db, FOCUS, "Living room router", "r:1").await;
        put(&mut db, &router, "subject_class", "device:network:router").await;
        put(&mut db, &router, "model", "R7000").await;
        put(&mut db, &router, "make", "Netgear").await;
        activate(&mut db, "case1", FOCUS, &router).await;

        let seeds = read(&mut db, Some(CHAT), &[SUBJECT_CONTEXT]).await;
        assert_eq!(
            seeds[SUBJECT_CONTEXT],
            "Device: Living room router (device:network:router)\nMake: Netgear\nModel: R7000"
        );
        assert_eq!(seeds.len(), 1, "only the asked-for name is produced");

        // No chat, or no active case: empty (the name always exists).
        assert_eq!(
            read(&mut db, None, &[SUBJECT_CONTEXT]).await[SUBJECT_CONTEXT],
            ""
        );
        assert_eq!(
            read(&mut db, Some("another-chat"), &[SUBJECT_CONTEXT]).await[SUBJECT_CONTEXT],
            ""
        );

        // A purge-marked, archived or another Focus's device reads as nothing.
        for status in ["user_deleted", "archived"] {
            sqlx::query("UPDATE entities SET status = ? WHERE id = ?")
                .bind(status)
                .bind(&router)
                .execute(&mut db.personal)
                .await
                .unwrap();
            assert_eq!(
                read(&mut db, Some(CHAT), &[SUBJECT_CONTEXT]).await[SUBJECT_CONTEXT],
                "",
                "{status}"
            );
        }
        sqlx::query("UPDATE entities SET status = 'active' WHERE id = ?")
            .bind(&router)
            .execute(&mut db.personal)
            .await
            .unwrap();
        sqlx::query("UPDATE entities SET owner_focus = ? WHERE id = ?")
            .bind(OTHER)
            .bind(&router)
            .execute(&mut db.personal)
            .await
            .unwrap();
        assert_eq!(
            read(&mut db, Some(CHAT), &[SUBJECT_CONTEXT]).await[SUBJECT_CONTEXT],
            "",
            "another Focus's device"
        );
    }

    #[tokio::test]
    async fn an_active_case_of_another_focus_or_an_invalid_pointer_is_handled() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let router = device(&mut db, FOCUS, "Router", "r:1").await;
        // A case another Focus opened, pointed at by this Focus's session variable.
        activate(&mut db, "theirs", OTHER, &router).await;
        put_session_variable(
            &mut db.outputs,
            &SessionWrite {
                context_key: CHAT,
                focus_id: FOCUS,
                name: ACTIVE_CASE_ID,
                value: "theirs",
                written_by_run: "run-2",
                expected_version: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            read(&mut db, Some(CHAT), &[SUBJECT_CONTEXT]).await[SUBJECT_CONTEXT],
            "",
            "only a case this Focus opened is its case"
        );
    }

    /// A Focus that opened a case on a device ANOTHER Focus owns still gets nothing:
    /// the device must be owned by the Focus that is reading.
    #[tokio::test]
    async fn a_case_on_another_focus_device_reads_as_nothing() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let router = device(&mut db, FOCUS, "Router", "r:1").await;
        put(&mut db, &router, "make", "Netgear").await;
        activate(&mut db, "ours", OTHER, &router).await;
        let seeds = read_device_seeds(
            &mut db.outputs,
            &mut db.personal,
            Some(CHAT),
            OTHER,
            &[SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF],
        )
        .await
        .unwrap()
        .0;
        assert_eq!(seeds[SUBJECT_CONTEXT], "", "OTHER does not own the router");
        assert_eq!(seeds[DEVICE_LIBRARY_BRIEF], "", "and lists none of it");
    }

    #[tokio::test]
    async fn the_brief_lists_this_focus_devices_newest_first_with_handles() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let old = device(&mut db, FOCUS, "Old modem", "r:1").await;
        let mid = device(&mut db, FOCUS, "Study laptop", "r:2").await;
        let new = device(&mut db, FOCUS, "Router", "r:3").await;
        device(&mut db, OTHER, "Camera", "r:4").await;
        for (id, at) in [
            (&old, "2026-01-01T00:00:00Z"),
            (&mid, "2026-02-01T00:00:00Z"),
            (&new, "2026-03-01T00:00:00Z"),
        ] {
            sqlx::query("UPDATE entities SET created_at = ? WHERE id = ?")
                .bind(at)
                .bind(id)
                .execute(&mut db.personal)
                .await
                .unwrap();
        }
        put(&mut db, &mid, "os_version", "Windows 11").await;
        put(&mut db, &new, "serial_number", "SN-1").await;
        put(&mut db, &new, "make", "Netgear").await;

        let brief =
            read(&mut db, None, &[DEVICE_LIBRARY_BRIEF]).await[DEVICE_LIBRARY_BRIEF].clone();
        let h = |id: &str| handles::entity_handle(FOCUS, id);
        assert_eq!(
            brief,
            format!(
                "- {}: Router | Make: Netgear\n- {}: Study laptop | Operating system: Windows 11\n- {}: Old modem",
                h(&new),
                h(&mid),
                h(&old)
            )
        );
        assert!(!brief.contains("Camera"), "another Focus's device");
        assert!(
            !brief.contains("SN-1"),
            "a not-permitted fact never appears in a list"
        );
        // No internal id anywhere.
        for id in [&old, &mid, &new] {
            assert!(!brief.contains(id.as_str()));
        }
    }

    #[tokio::test]
    async fn the_brief_caps_at_eight_devices_and_counts_the_rest() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        for i in 0..10 {
            device(&mut db, FOCUS, &format!("Device {i}"), &format!("r:{i}")).await;
        }
        let brief =
            read(&mut db, None, &[DEVICE_LIBRARY_BRIEF]).await[DEVICE_LIBRARY_BRIEF].clone();
        assert_eq!(brief.lines().count(), 9);
        assert!(brief.ends_with("- and 2 more"));
    }

    #[tokio::test]
    async fn both_names_always_exist_even_with_nothing_stored() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let seeds = read(
            &mut db,
            Some(CHAT),
            &[SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF],
        )
        .await;
        assert_eq!(seeds[SUBJECT_CONTEXT], "");
        assert_eq!(seeds[DEVICE_LIBRARY_BRIEF], "");
    }

    /// A brief that a handle in it resolves back to the same device, and only for
    /// this Focus: the text a model is shown is the text it can act on.
    #[tokio::test]
    async fn a_handle_in_the_brief_resolves_to_its_device_for_this_focus_only() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let router = device(&mut db, FOCUS, "Router", "r:1").await;
        let brief =
            read(&mut db, None, &[DEVICE_LIBRARY_BRIEF]).await[DEVICE_LIBRARY_BRIEF].clone();
        let handle = brief
            .strip_prefix("- ")
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(
            handles::resolve_entity_handle(&mut db.personal, FOCUS, &handle)
                .await
                .unwrap(),
            router
        );
        assert!(
            handles::resolve_entity_handle(&mut db.personal, OTHER, &handle)
                .await
                .is_err()
        );
    }

    // -- no write, no other names ----------------------------------------------------

    #[test]
    fn only_the_two_device_names_are_device_seeds() {
        for n in spec::NAMES {
            assert_eq!(
                is_device_seed(n),
                *n == SUBJECT_CONTEXT || *n == DEVICE_LIBRARY_BRIEF,
                "{n}"
            );
        }
    }

    #[test]
    fn this_module_only_reads() {
        let src = include_str!("device_seeding.rs");
        let prod = src.split("\n#[cfg(test)]").next().unwrap();
        for forbidden in [
            "insert into",
            "update ",
            "delete from",
            "put_owned_fact",
            "create_subject",
            "put_session_variable",
            "sqlx::query",
        ] {
            let hits: Vec<&str> = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.to_lowercase().contains(forbidden))
                .collect();
            assert!(hits.is_empty(), "`{forbidden}`: {hits:?}");
        }
    }

    // -- Presence, the spec, and the seed pass ------------------------------------

    #[test]
    fn both_names_are_derived_always_present_authored_text() {
        for name in [SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF] {
            let s = spec::spec(name).expect("registered");
            assert_eq!(s.source, spec::Source::Derived, "{name}");
            assert_eq!(s.presence, spec::Presence::Always, "{name}");
            assert_eq!(s.literals, spec::Literals::Authored, "{name}");
            // Validated as text: empty is valid (nothing), a NUL or an oversized value is not.
            assert!((s.validate)(""), "{name}");
            assert!(!(s.validate)("a\0b"), "{name}");
            assert!(
                !(s.validate)(&"a".repeat(spec::DERIVED_TEXT_MAX_BYTES + 1)),
                "{name}"
            );
            assert!(spec::NAMES.contains(&name));
        }
    }

    #[tokio::test]
    async fn the_outputs_reader_does_not_silently_skip_a_device_name() {
        // read_seeds is the outputs.db-only reader: handing it a device name fails
        // closed (an Always name that did not come back), never an absent value.
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        for name in [SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF] {
            let r = crate::conductor::seeding::read_seeds(
                &mut db.outputs,
                Some(CHAT),
                FOCUS,
                &[name],
                chrono::Utc::now(),
            )
            .await;
            assert_eq!(r.unwrap_err(), SeedError::InvalidValue(name));
        }
    }

    fn step_naming(id: &str, template: &str) -> crate::conductor::tokens::ActionDefinition {
        use crate::conductor::tokens::{ActionDefinition, ExternalAccess, StepType};
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
            revisitable: true,
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

    /// A pass reads a device name only if a template, declared input or guard
    /// names it, seeds both through the production seed_pass, and a pass that names
    /// neither produces neither.
    #[tokio::test]
    async fn a_pass_reads_exactly_the_device_names_it_asks_for() {
        use crate::conductor::seeding::{required_names, seed_pass};
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let router = device(&mut db, FOCUS, "Router", "r:1").await;
        put(&mut db, &router, "make", "Netgear").await;
        activate(&mut db, "case1", FOCUS, &router).await;

        let both = vec![step_naming("a", "{subject_context} {device_library_brief}")];
        assert_eq!(
            required_names(&both),
            [SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF]
        );
        let only_brief = vec![step_naming("a", "{device_library_brief}")];
        assert_eq!(required_names(&only_brief), [DEVICE_LIBRARY_BRIEF]);
        let other = vec![step_naming("a", "{case_context}")];
        assert_eq!(required_names(&other), ["case_context"]);

        let rows = |s: &[crate::conductor::tokens::ActionDefinition]| {
            s.iter().map(|x| x.output_var.clone()).collect::<Vec<_>>()
        };
        let t = seed_pass(
            USER,
            PERSONA,
            KEY,
            Some(CHAT),
            FOCUS,
            &both,
            &rows(&both),
            &LaunchResolution::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            t.seeds[SUBJECT_CONTEXT], "Device: Router\nMake: Netgear",
            "the active case's device"
        );
        assert!(t.seeds[DEVICE_LIBRARY_BRIEF].contains(": Router | Make: Netgear"));
        // Only the asked-for names are seeded, alone and beside another derived name.
        let t = seed_pass(
            USER,
            PERSONA,
            KEY,
            Some(CHAT),
            FOCUS,
            &only_brief,
            &rows(&only_brief),
            &LaunchResolution::default(),
        )
        .await
        .unwrap();
        assert_eq!(t.seeds.len(), 1);
        let t = seed_pass(
            USER,
            PERSONA,
            KEY,
            Some(CHAT),
            FOCUS,
            &other,
            &rows(&other),
            &LaunchResolution::default(),
        )
        .await
        .unwrap();
        assert_eq!(t.seeds.keys().collect::<Vec<_>>(), ["case_context"]);
        // Every bound name, device names included, is restricted: a step that does
        // not name one never receives it.
        assert!(t.restricted.contains(SUBJECT_CONTEXT));
        assert!(t.restricted.contains(DEVICE_LIBRARY_BRIEF));
        // A Focus that names no bound name opens no store.
        let none = vec![step_naming("a", "{user_input}")];
        let t = seed_pass(
            USER,
            PERSONA,
            KEY,
            None,
            FOCUS,
            &none,
            &rows(&none),
            &LaunchResolution::default(),
        )
        .await
        .unwrap();
        assert!(t.seeds.is_empty());
    }

    // -- items.id=924: the device seed -------------------------------------------

    fn sv(entity: &str, field: &str, value: &str, d: Disclosure) -> SeededValue {
        SeededValue {
            entity_id: entity.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
            disclosure: d,
            access_credential: field == "serial_number",
            suggestion: None,
        }
    }

    /// The fingerprint: none for an empty seed (no pin entry), stable for the same
    /// values in any order, and different when a value, its disclosure, a device or the
    /// brief's count changes. Debug and the fingerprint never hold a value.
    #[test]
    fn the_fingerprint_changes_with_every_seeded_part_and_holds_no_value() {
        assert_eq!(DeviceSeed::default().fingerprint(), None);
        let base = vec![
            sv("e1", DEVICE_NAME_FIELD, "Study laptop", Disclosure::Ask),
            sv("e1", "serial_number", "SECRET-SN-1", Disclosure::Ask),
            sv("e1", "make", "Lenovo", Disclosure::Pass),
        ];
        let fp = DeviceSeed::for_test(base.clone(), Some(1))
            .fingerprint()
            .unwrap();
        let mut rev = base.clone();
        rev.reverse();
        assert_eq!(
            DeviceSeed::for_test(rev, Some(1)).fingerprint().unwrap(),
            fp
        );
        let mut edited = base.clone();
        edited[1].value = "SECRET-SN-2".to_owned();
        let mut redisclosed = base.clone();
        redisclosed[2].disclosure = Disclosure::Ask;
        let mut added = base.clone();
        added.push(sv("e2", DEVICE_NAME_FIELD, "Router", Disclosure::Ask));
        let mut renamed = base.clone();
        renamed[0].value = "Old laptop".to_owned();
        for (what, seed) in [
            ("edited", DeviceSeed::for_test(edited, Some(1))),
            ("re-disclosed", DeviceSeed::for_test(redisclosed, Some(1))),
            ("added", DeviceSeed::for_test(added, Some(2))),
            ("renamed", DeviceSeed::for_test(renamed, Some(1))),
            ("count", DeviceSeed::for_test(base.clone(), Some(9))),
        ] {
            assert_ne!(seed.fingerprint().unwrap(), fp, "{what}");
        }
        assert!(!fp.contains("SECRET"));
        let d = format!("{:?}", DeviceSeed::for_test(base, Some(1)));
        assert!(!d.contains("SECRET") && !d.contains("Study"), "{d}");
        // A brief that found no device seeded nothing: no pin entry.
        assert_eq!(DeviceSeed::for_test(vec![], Some(0)).fingerprint(), None);
    }

    #[test]
    fn the_device_fact_key_names_the_entity_field_and_value() {
        let k = device_fact_key("e1", "serial_number", "SN-1");
        assert!(k.starts_with("device_fact:") && !k.contains("SN-1"));
        assert_eq!(k, device_fact_key("e1", "serial_number", " SN-1 "));
        assert_ne!(k, device_fact_key("e1", "serial_number", "SN-2"));
        assert_ne!(k, device_fact_key("e2", "serial_number", "SN-1"));
        assert_ne!(k, device_fact_key("e1", "retailer", "SN-1"));
    }

    /// A pass seeds the name and every catalogued fact of each device it reads, with the
    /// catalogue's disclosure, the flag and the suggestion; the subject class is a fact
    /// like any other here.
    #[tokio::test]
    async fn a_pass_seeds_each_read_devices_values_with_their_disclosure() {
        let _root = TestRoot::new().await;
        let mut db = dbs().await;
        let laptop = device(&mut db, FOCUS, "Study laptop", "l:1").await;
        put(&mut db, &laptop, "make", "Lenovo").await;
        put(&mut db, &laptop, "serial_number", "PF2ABC3D").await;
        activate(&mut db, "c1", FOCUS, &laptop).await;
        let (_, seed) = read_device_seeds(
            &mut db.outputs,
            &mut db.personal,
            Some(CHAT),
            FOCUS,
            &[SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF],
        )
        .await
        .unwrap();
        let mut got: Vec<(&str, &str, Disclosure, bool, Option<&str>)> = seed
            .values()
            .iter()
            .map(|v| {
                assert_eq!(v.entity_id, laptop);
                (
                    v.field.as_str(),
                    v.value.as_str(),
                    v.disclosure,
                    v.access_credential,
                    v.suggestion.as_deref(),
                )
            })
            .collect();
        got.sort_by_key(|g| g.0);
        assert_eq!(
            got,
            [
                (
                    DEVICE_NAME_FIELD,
                    "Study laptop",
                    Disclosure::Ask,
                    false,
                    Some("[device]")
                ),
                ("make", "Lenovo", Disclosure::Pass, false, None),
                (
                    "serial_number",
                    "PF2ABC3D",
                    Disclosure::Ask,
                    true,
                    Some("[serial number]")
                ),
            ]
        );
        // Read twice, nothing changed: the same pin.
        let (_, again) = read_device_seeds(
            &mut db.outputs,
            &mut db.personal,
            Some(CHAT),
            FOCUS,
            &[SUBJECT_CONTEXT, DEVICE_LIBRARY_BRIEF],
        )
        .await
        .unwrap();
        assert_eq!(seed.fingerprint(), again.fingerprint());
        // A pass naming no device name seeds nothing.
        let (_, none) =
            read_device_seeds(&mut db.outputs, &mut db.personal, Some(CHAT), FOCUS, &[])
                .await
                .unwrap();
        assert!(none.is_empty());
    }

    // -- items.id=924 unit 3: the external rendering ------------------------------

    /// THE WORDING of the RMA fixture, local and external, pinned. The external text
    /// keeps the skeleton and every pass and ask fact, and drops the never fact (the
    /// fixture makes the warranty contact never); the local text is unchanged.
    #[test]
    fn the_external_subject_context_drops_never_facts_and_keeps_the_rest() {
        let cat = catalogue_with_never_for_test("warranty_contact");
        let mut seed = DeviceSeed::default();
        let local = seed.add_for_test(
            &cat,
            &entity_for_test("ent-1", "Study laptop", false, false),
            &rma_facts_for_test(),
            true,
        );
        assert_eq!(
            local,
            "Device: Study laptop (device:endpoint:laptop)\n\
             Make: Lenovo\n\
             Model: ThinkPad X1\n\
             Bought on: 2025-03-14\n\
             Serial number: PF2ABC3D\n\
             Warranty contact: help@warranty.example"
        );
        assert_eq!(
            seed.external_subject_context(),
            "Device: Study laptop (device:endpoint:laptop)\n\
             Make: Lenovo\n\
             Model: ThinkPad X1\n\
             Bought on: 2025-03-14\n\
             Serial number: PF2ABC3D"
        );
        // With the shipped catalogue (warranty contact ask) it is rendered too.
        let mut shipped = DeviceSeed::default();
        let local = shipped.add_for_test(
            device_fields::registry().unwrap(),
            &entity_for_test("ent-1", "Study laptop", false, false),
            &rma_facts_for_test(),
            true,
        );
        assert_eq!(shipped.external_subject_context(), local);
        // No device: empty, like the local text.
        assert_eq!(DeviceSeed::default().external_subject_context(), "");
        assert_eq!(DeviceSeed::default().external_device_brief(), "");
    }

    /// decisions.id=513 at the Boundary: a REDACTED device shows "[device]" and its pass
    /// facts only, and its name and ask values become never (removed by the carry scan,
    /// never asked); a SUPPRESSED device renders nothing and the same values are never.
    #[test]
    fn a_redacted_or_suppressed_device_is_withheld_outside_and_its_values_are_never() {
        let cat = device_fields::registry().unwrap();
        let mut redacted = DeviceSeed::default();
        redacted.add_for_test(
            cat,
            &entity_for_test("ent-1", "Study laptop", true, false),
            &rma_facts_for_test(),
            true,
        );
        assert_eq!(
            redacted.external_subject_context(),
            "Device: [device] (device:endpoint:laptop)\nMake: Lenovo\nModel: ThinkPad X1"
        );
        let mut suppressed = DeviceSeed::default();
        suppressed.add_for_test(
            cat,
            &entity_for_test("ent-1", "Study laptop", false, true),
            &rma_facts_for_test(),
            true,
        );
        assert_eq!(suppressed.external_subject_context(), "");
        for seed in [&redacted, &suppressed] {
            for v in seed.values() {
                let want = if ["make", "model", "subject_class"].contains(&v.field.as_str()) {
                    Disclosure::Pass
                } else {
                    Disclosure::Never
                };
                assert_eq!(v.disclosure, want, "{}", v.field);
            }
        }
        // A flag change changes the pin.
        let mut visible = DeviceSeed::default();
        visible.add_for_test(
            cat,
            &entity_for_test("ent-1", "Study laptop", false, false),
            &rma_facts_for_test(),
            true,
        );
        assert_ne!(visible.fingerprint(), redacted.fingerprint());
    }

    /// The external brief: a suppressed device left out, a redacted one named
    /// "[device]", a visible one as locally (pass facts only).
    #[test]
    fn the_external_brief_withholds_by_visibility() {
        let cat = device_fields::registry().unwrap();
        let mut seed = DeviceSeed::default();
        for (id, name, redact, hide) in [
            ("a", "Study laptop", false, false),
            ("b", "Kitchen tablet", true, false),
            ("c", "Hidden router", false, true),
        ] {
            seed.add_for_test(
                cat,
                &entity_for_test(id, name, redact, hide),
                &rma_facts_for_test(),
                false,
            );
        }
        assert_eq!(
            seed.external_device_brief(),
            "- E-a: Study laptop (device:endpoint:laptop) | Make: Lenovo | Model: ThinkPad X1\n\
             - E-b: [device] (device:endpoint:laptop) | Make: Lenovo | Model: ThinkPad X1"
        );
    }
}

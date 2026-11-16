// src-tauri/src/conductor/device_seeding.rs
//
// items.id=891 stage 6 (decisions.id=516, 509, 940): the two DERIVED bound names
// that put a Focus-owned device in front of a model, subject_context and
// device_library_brief (bound_spec.rs: Source::Derived, Presence::Always, text).
// Like case_context and the other derived names they are READ at pass start
// (seeding::seed_pass), never written, never a TaskStep, and never in the
// extraction input: every row that names one is tainted (conductor::taint), and an
// external step may not name or declare either, whatever STORE_NAMES_EXTERNAL_ALLOWED
// says (items.id=884 flipped that flag to true and kept these two out by a closed
// list, declared_inputs::DEVICE_NAMES_NEVER_EXTERNAL, with an explicit LOAD refusal).
// Both are LOCAL prompt text.
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

use std::collections::HashMap;

use sqlx::SqliteConnection;

use crate::conductor::bound_spec::{
    self as spec, ACTIVE_CASE_ID, DEVICE_LIBRARY_BRIEF, SUBJECT_CONTEXT,
};
use crate::conductor::device_fields::{self, Catalogue, SUBJECT_CLASS_FIELD};
use crate::conductor::seeding::{cap, clean, SeedError};
use crate::persistence::case_store;
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

/// True for the two names this module reads.
pub fn is_device_seed(name: &str) -> bool {
    name == SUBJECT_CONTEXT || name == DEVICE_LIBRARY_BRIEF
}

// ---------------------------------------------------------------------------
// Rendering (pure, so the exact text is testable)
// ---------------------------------------------------------------------------

/// One device as the brief shows it: its handle, name, class and every active
/// catalogued fact (label, value, tier-2 policy), in catalogue order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefDevice {
    pub handle: String,
    pub name: String,
    pub class: Option<String>,
    /// (label, value, abstraction_tier2), catalogue order, subject_class excluded.
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
                .filter(|(_, _, tier2)| tier2 == "pass")
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

/// The catalogued facts of a device as (label, value, tier-2 policy) in catalogue
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
            rest.push((def.label.clone(), f.value, def.abstraction_tier2.clone()));
        }
    }
    (class, rest)
}

/// Read the device seeds named in `names` (a subset of subject_context and
/// device_library_brief). Every value must satisfy its own spec, and any store
/// read error fails the pass closed, like every other derived name.
pub async fn read_device_seeds(
    outputs: &mut SqliteConnection,
    personal: &mut SqliteConnection,
    context_key: Option<&str>,
    focus_id: &str,
    names: &[&'static str],
) -> Result<HashMap<String, String>, SeedError> {
    let catalogue = device_fields::registry().map_err(|e| store_err(SUBJECT_CONTEXT, e))?;
    let mut out: HashMap<String, String> = HashMap::new();

    if names.contains(&SUBJECT_CONTEXT) {
        out.insert(
            SUBJECT_CONTEXT.to_owned(),
            subject_context(outputs, personal, catalogue, context_key, focus_id).await?,
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
            let (class, facts) = split_facts(catalogue, facts);
            shown.push(BriefDevice {
                handle: handles::entity_handle(focus_id, &d.id),
                name: d.display_name.clone(),
                class,
                facts,
            });
        }
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
    Ok(out)
}

async fn subject_context(
    outputs: &mut SqliteConnection,
    personal: &mut SqliteConnection,
    catalogue: &Catalogue,
    context_key: Option<&str>,
    focus_id: &str,
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
    let (class, facts) = split_facts(catalogue, facts);
    let facts: Vec<(String, String)> = facts.into_iter().map(|(l, v, _)| (l, v)).collect();
    Ok(render_subject_context(
        &entity.display_name,
        class.as_deref(),
        &facts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        .unwrap();
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
        let t = seed_pass(USER, PERSONA, KEY, Some(CHAT), FOCUS, &both, &rows(&both))
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
        )
        .await
        .unwrap();
        assert_eq!(t.seeds.len(), 1);
        let t = seed_pass(USER, PERSONA, KEY, Some(CHAT), FOCUS, &other, &rows(&other))
            .await
            .unwrap();
        assert_eq!(t.seeds.keys().collect::<Vec<_>>(), ["case_context"]);
        // Every bound name, device names included, is restricted: a step that does
        // not name one never receives it.
        assert!(t.restricted.contains(SUBJECT_CONTEXT));
        assert!(t.restricted.contains(DEVICE_LIBRARY_BRIEF));
        // A Focus that names no bound name opens no store.
        let none = vec![step_naming("a", "{user_input}")];
        let t = seed_pass(USER, PERSONA, KEY, None, FOCUS, &none, &rows(&none))
            .await
            .unwrap();
        assert!(t.seeds.is_empty());
    }
}

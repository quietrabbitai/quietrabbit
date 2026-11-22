// src-tauri/src/conductor/model_block/admission_tests.rs
//
// items.id=894 part 1 (decisions.id=941 point 11, decisions.id=942 point 11): the
// ADMISSION TEST over the block registry. A block is admitted to the registry only
// if it passes every check here, for every id in REGISTERED_STABLE_IDS, so a new
// block that skips one fails CI instead of waiting for a review to notice:
//
//   1  its kind is consistent (a pure block has a handler and no spec, a model
//      block a spec and no handler, a render block (items.id=387) a render spec and
//      neither, and no id is two kinds)
//   2  it is a mirror row (shared_043.sql adds the model blocks) and a member of
//      the building_blocks mirror as inline_composable
//   3  its catalog row, from the checked-in export of qr_docs.db, has the floor
//      declaration, capability toggles and customization notes filled in
//   4  its declared configuration schema accepts its example, and rejects the
//      example with an unknown key at every level, with each required key removed,
//      and with each closed vocabulary set to a value outside it
//   5  it names an output variable, which is not a system token or a bound name
//   6  a Focus built from it loads through the REAL loader (lint, guard and
//      placement analysis included)
//   7  a pure block leaves the track alone and leaks no input text, and a model
//      block expands to an ordinary local step (no block_stable_id, a requirement,
//      no handoff, no {user_input} or {persona_context})
//   8  the hosted rule: allowing hosted_api and naming {user_input} or
//      {persona_context} is refused
//   9  tighten() never widens an envelope (property test, config.rs) and no
//      configuration key can switch a gate off (the pinned key list below)
//  10  the resolver can serve it (items.id=896): every shipped document type has at
//      least one rated local cell in the shipped capability data or is named in
//      UNRATED_SHIPPED_TYPES (so adding a type forces a decision about its
//      rating), every model block's task type has at least one cell or is named
//      in UNRATED_BLOCK_TASK_TYPES, neither list holds an entry that is rated or
//      unused, and every local cell names a curated tag
//
// The catalog check reads a checked-in file, not qr_docs.db: the running app and
// CI never query the coordination database. Code re-exports the file (items.id=895)
// from qr_docs.db after Chat-PM has written the catalog rows for a new block.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::conductor::block_dispatch::{
    block_handler, block_kind, registered_block, RegisteredBlock, PURE_STABLE_IDS,
    REGISTERED_STABLE_IDS, RENDER_STABLE_IDS,
};
use crate::conductor::model_block::config::{tighten, Envelope, EnvelopeOverride};
use crate::conductor::model_block::document_type::{
    registry, DocumentType, Settings, SettingsPatch,
};
use crate::conductor::model_block::{
    BlockKind, ExpandInput, EXTERNAL_MODEL_BLOCKS_ALLOWED, MODEL_STABLE_IDS,
};
use crate::conductor::tokens::{is_system_token, ExternalAccess, StepType};
use crate::conductor::types::{TaskStep, TaskTrack};

const CATALOG: &str = include_str!("../../../tests/fixtures/building_blocks_catalog.json");

// ---------------------------------------------------------------------------
// Every JSON key a model block's configuration may carry. A test serialises a
// maximal example of each block, the document type and the two override shapes,
// and requires set equality with this list: adding a field forces a reviewed edit
// of it. No key may name a gate, a scan, a review, consent, an audit, a bypass, a
// skip or a disable (FORBIDDEN_SUBSTRINGS), so no configuration field can switch
// a gate off.
// ---------------------------------------------------------------------------

const PINNED_KEYS: &[&str] = &[
    "based_on",
    "brief_var",
    "change_var",
    "citation_markers",
    "class_preference",
    "common",
    "context_fields",
    "context_var",
    "declared_inputs",
    "destinations",
    "difficulty",
    "directness",
    "display_name",
    "document_type",
    "draft",
    "draft_var",
    "flag_unsupplied_references",
    "form",
    "formality",
    "format_note",
    "highest_tier",
    "id",
    "input_var",
    "latitude",
    "length",
    "length_words",
    "max",
    "min",
    "mode",
    "name",
    "need",
    "output_size",
    "output_var",
    "pacing",
    "params",
    "quality_floor",
    "range",
    "revise",
    "sender",
    "sender_var",
    "settings",
    "spec",
    "task_type",
    "temperature",
    "template",
    "tone",
    "top_p",
    "voice",
];

const FORBIDDEN_SUBSTRINGS: &[&str] = &[
    "gate",
    "scan",
    "review",
    "consent",
    "audit",
    "bypass",
    "skip",
    "disable",
    "override_tier",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A valid customization of a registered block, whatever its kind.
fn example_of(id: &str) -> Value {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.example)(),
        RegisteredBlock::Model(s) => (s.example)(),
        RegisteredBlock::Render(s) => (s.example)(),
    }
}

/// Parse and validate a customization the way the loader does.
fn check(id: &str, v: &Value) -> Result<(), String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.validate)(v),
        RegisteredBlock::Model(s) => (s.parse)(v, EXTERNAL_MODEL_BLOCKS_ALLOWED).map(|_| ()),
        RegisteredBlock::Render(s) => (s.parse)(v).map(|_| ()),
    }
}

/// (JSON pointer of an object, key) for every key of every object in `v`.
fn key_paths(v: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.push((prefix.to_owned(), k.clone()));
                key_paths(child, &format!("{prefix}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                key_paths(child, &format!("{prefix}/{i}"), out);
            }
        }
        _ => {}
    }
}

fn all_keys(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.insert(k.clone());
                all_keys(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|c| all_keys(c, out)),
        _ => {}
    }
}

/// The variable names a block's example reads: the names a test Focus must
/// produce earlier. A system token (voice_profile) needs no producer.
fn input_names(id: &str, ex: &Value) -> Vec<String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(_) => vec![ex["input_var"].as_str().unwrap().to_owned()],
        RegisteredBlock::Model(_) => ex["common"]["declared_inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_owned())
            .filter(|n| !is_system_token(n))
            .collect(),
        RegisteredBlock::Render(_) => ex["declared_inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_owned())
            .filter(|n| !is_system_token(n))
            .collect(),
    }
}

fn output_var_of(id: &str, ex: &Value) -> Option<String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.output_var)(ex),
        RegisteredBlock::Model(_) => ex["common"]["output_var"].as_str().map(str::to_owned),
        RegisteredBlock::Render(s) => (s.output_var)(ex),
    }
}

// ---------------------------------------------------------------------------
// 1. Kind consistency
// ---------------------------------------------------------------------------

#[test]
fn every_registered_id_has_exactly_one_kind_and_the_lists_agree() {
    let mut pure = BTreeSet::new();
    let mut model = BTreeSet::new();
    let mut render = BTreeSet::new();
    for id in REGISTERED_STABLE_IDS {
        let kind = block_kind(id).unwrap_or_else(|| panic!("{id} is listed but not registered"));
        match (registered_block(id).unwrap(), kind) {
            (RegisteredBlock::Pure(h), BlockKind::Pure) => {
                assert_eq!(h.stable_id, *id);
                assert!(crate::conductor::model_block::model_block_spec(id).is_none());
                pure.insert(*id);
            }
            (RegisteredBlock::Model(s), BlockKind::Model) => {
                assert_eq!(s.stable_id, *id);
                assert!(
                    block_handler(id).is_none(),
                    "{id} must not also have a handler"
                );
                model.insert(*id);
            }
            (RegisteredBlock::Render(s), BlockKind::Render) => {
                assert_eq!(s.stable_id, *id);
                assert!(block_handler(id).is_none(), "{id} must not have a handler");
                assert!(
                    crate::conductor::model_block::model_block_spec(id).is_none(),
                    "{id} must not also be a model block"
                );
                render.insert(*id);
            }
            _ => panic!("{id}: kind does not match its registration"),
        }
    }
    assert_eq!(
        render,
        RENDER_STABLE_IDS.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        pure,
        PURE_STABLE_IDS.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        model,
        MODEL_STABLE_IDS.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        pure.len() + model.len() + render.len(),
        REGISTERED_STABLE_IDS.len()
    );
    // No seed block is registered yet: its contract belongs to B7a.
    assert!(REGISTERED_STABLE_IDS
        .iter()
        .all(|id| block_kind(id) != Some(BlockKind::Seed)));
    // Nothing unregistered resolves.
    for id in ["cb-01", "cb-02", "cb-10", "pc-04", "cb-16", "nope", ""] {
        assert!(registered_block(id).is_none(), "{id}");
    }
}

// ---------------------------------------------------------------------------
// 3. Catalog row
// ---------------------------------------------------------------------------

fn filled(v: &Value, key: &str) -> bool {
    v[key]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.trim() != "None")
}

#[test]
fn every_registered_block_has_a_filled_catalog_row_in_the_checked_in_export() {
    let export: Value = serde_json::from_str(CATALOG).expect("catalog export must parse");
    let rows = export["rows"].as_array().expect("rows");
    for id in REGISTERED_STABLE_IDS {
        let row = rows
            .iter()
            .find(|r| r["stable_id"] == *id)
            .unwrap_or_else(|| panic!("{id} has no catalog row in the export"));
        for col in [
            "floor_declaration",
            "capability_toggles",
            "generic_customization_notes",
        ] {
            assert!(filled(row, col), "{id}: catalog column {col} is empty");
        }
        assert_eq!(row["invocation_mode"], "inline_composable", "{id}");
        let want = match block_kind(id).unwrap() {
            BlockKind::Pure => "pure",
            BlockKind::Model => "model",
            BlockKind::Render => "render",
            BlockKind::Seed => "seed",
        };
        assert_eq!(
            row["kind"], want,
            "{id}: the export records a different kind"
        );
    }
    // The export carries no row for a block that is not registered.
    for row in rows {
        let id = row["stable_id"].as_str().unwrap();
        assert!(
            REGISTERED_STABLE_IDS.contains(&id),
            "{id} is in the export but not registered"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Schema accepts the example and rejects every mutation
// ---------------------------------------------------------------------------

/// Keys whose value is a closed vocabulary: set to a value outside it, the
/// configuration must stop parsing.
const VOCABULARY_KEYS: &[&str] = &[
    "task_type",
    "difficulty",
    "output_size",
    "class_preference",
    "highest_tier",
    "min",
    "max",
    "mode",
    "kind",
    "on_fail",
    "document_type",
];

#[test]
fn every_example_validates_and_every_mutation_of_it_fails() {
    for id in REGISTERED_STABLE_IDS {
        let ex = example_of(id);
        check(id, &ex).unwrap_or_else(|e| panic!("{id}: its own example must validate: {e}"));

        let mut keys = Vec::new();
        key_paths(&ex, "", &mut keys);
        assert!(!keys.is_empty());

        // An unknown key at every object, the root included.
        let mut objects: BTreeSet<String> = keys.iter().map(|(p, _)| p.clone()).collect();
        objects.insert(String::new());
        for ptr in &objects {
            let mut m = ex.clone();
            m.pointer_mut(ptr)
                .and_then(Value::as_object_mut)
                .unwrap_or_else(|| panic!("{id}: {ptr} is not an object"))
                .insert("zz_unknown".to_owned(), json!(1));
            assert!(
                check(id, &m).is_err(),
                "{id}: an unknown key at '{ptr}' was accepted"
            );
        }

        // Each required key removed (a null example value marks an optional one).
        for (ptr, key) in &keys {
            if ex.pointer(ptr).unwrap()[key].is_null() {
                continue;
            }
            let mut m = ex.clone();
            m.pointer_mut(ptr)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                check(id, &m).is_err(),
                "{id}: '{key}' at '{ptr}' was removed and accepted"
            );
        }

        // Each closed vocabulary set outside it.
        for (ptr, key) in &keys {
            let v = &ex.pointer(ptr).unwrap()[key];
            if VOCABULARY_KEYS.contains(&key.as_str()) && v.is_string() {
                let mut m = ex.clone();
                m.pointer_mut(ptr).unwrap()[key.as_str()] = json!("zz_bad");
                assert!(
                    check(id, &m).is_err(),
                    "{id}: '{key}' = zz_bad at '{ptr}' was accepted"
                );
            }
        }
        if ex.pointer("/common/range/destinations").is_some() {
            let mut m = ex.clone();
            m["common"]["range"]["destinations"] = json!(["zz_bad"]);
            assert!(
                check(id, &m).is_err(),
                "{id}: an unknown destination was accepted"
            );
        }

        // Not an object at all, and empty.
        assert!(check(id, &json!([1, 2])).is_err());
        assert!(check(id, &json!({})).is_err());
    }
}

// ---------------------------------------------------------------------------
// 5. Output variable
// ---------------------------------------------------------------------------

#[test]
fn every_block_names_an_output_variable_that_is_not_reserved() {
    for id in REGISTERED_STABLE_IDS {
        let out =
            output_var_of(id, &example_of(id)).unwrap_or_else(|| panic!("{id}: no output_var"));
        assert!(!out.is_empty());
        assert!(!is_system_token(&out), "{id}: {out} is a system token");
        assert!(
            !crate::conductor::declared_inputs::bound_names().contains(&out.as_str()),
            "{id}: {out} is a bound name"
        );
        // A model block that names a reserved output is refused by its own parser.
        if block_kind(id) == Some(BlockKind::Model) {
            for reserved in [
                "user_input",
                "persona_context",
                "voice_profile",
                "previous_output",
            ] {
                let mut m = example_of(id);
                m["common"]["output_var"] = json!(reserved);
                assert!(
                    check(id, &m).is_err(),
                    "{id}: output_var {reserved} accepted"
                );
            }
            for bound in crate::conductor::declared_inputs::bound_names() {
                let mut m = example_of(id);
                m["common"]["output_var"] = json!(bound);
                assert!(check(id, &m).is_err(), "{id}: output_var {bound} accepted");
            }
        }
        // items.id=387: a render block's own parser refuses the same names.
        if block_kind(id) == Some(BlockKind::Render) {
            for reserved in [
                "user_input",
                "persona_context",
                "voice_profile",
                "previous_output",
            ] {
                let mut m = example_of(id);
                m["output_var"] = json!(reserved);
                assert!(
                    check(id, &m).is_err(),
                    "{id}: output_var {reserved} accepted"
                );
            }
            for bound in crate::conductor::declared_inputs::bound_names() {
                let mut m = example_of(id);
                m["output_var"] = json!(bound);
                assert!(check(id, &m).is_err(), "{id}: output_var {bound} accepted");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Loads through the real loader (and 2: the mirror)
// ---------------------------------------------------------------------------

struct TestEnv {
    _tempdir: tempfile::TempDir,
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved_root: Option<String>,
    pool: sqlx::SqlitePool,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match &self.saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

async fn setup() -> TestEnv {
    let lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db migration");
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool");
    TestEnv {
        _tempdir: tempdir,
        _lock: lock,
        saved_root,
        pool,
    }
}

/// One composition row: (action_id, block_stable_id, customization).
type Row = (String, Option<String>, Value);

async fn seed_focus(pool: &sqlx::SqlitePool, focus_id: &str, ceiling: &str, rows: &[Row]) {
    use crate::persistence::focus_composition_store as store;
    let seed = store::FocusSeed {
        focus: store::FocusSeedRow {
            focus_id: focus_id.to_owned(),
            display_name: "Admission Focus".to_owned(),
            description: String::new(),
            version: "1.0".to_owned(),
            max_routing_tier: ceiling.to_owned(),
            output_type: "general".to_owned(),
            suggest_in_focuses: Vec::new(),
            multi_source_validation: false,
            generic_title_template: "Hidden item".to_owned(),
            high_priority_trigger_anchor_field: None,
            high_priority_trigger_offset: None,
            status: "shipped".to_owned(),
            collection_posture: "per_field".to_owned(),
        },
        compositions: rows
            .iter()
            .map(|(action, block, custom)| {
                store::CompositionSeed::plain(action, block.as_deref(), &custom.to_string())
            })
            .collect(),
        questions: Vec::new(),
        ui_one_liner: None,
    };
    let mut conn = pool.acquire().await.unwrap();
    store::write_focus(&mut conn, &seed).await.unwrap();
}

fn launch_question(
    question_id: &str,
    slot: &str,
    kind: &str,
    asked: bool,
    default_option: &str,
    prompt: &str,
    options: &[(&str, Option<&str>)],
) -> crate::persistence::focus_composition_store::LaunchQuestionSeed {
    use crate::persistence::focus_composition_store as store;
    store::LaunchQuestionSeed {
        question_id: question_id.to_owned(),
        slot: slot.to_owned(),
        kind: kind.to_owned(),
        asked,
        default_option: default_option.to_owned(),
        prompt: prompt.to_owned(),
        options: options
            .iter()
            .map(|(o, envelope)| store::LaunchOptionSeed {
                option_id: (*o).to_owned(),
                label: "Label".to_owned(),
                hint: String::new(),
                envelope: envelope.map(str::to_owned),
            })
            .collect(),
    }
}

/// A document_type launch question on `focus_id` (items.id=897): `fixed` makes it a
/// preset (asked = 0, one option), else an asked question with two options. The
/// default is rma_letter.
async fn seed_document_type_question(pool: &sqlx::SqlitePool, focus_id: &str, fixed: bool) {
    let options: &[(&str, Option<&str>)] = if fixed {
        &[("rma_letter", None)]
    } else {
        &[("rma_letter", None), ("cover_letter", None)]
    };
    let q = launch_question(
        "kind",
        "document_type",
        "choice",
        !fixed,
        "rma_letter",
        "Which kind?",
        options,
    );
    let mut conn = pool.acquire().await.unwrap();
    crate::persistence::focus_composition_store::write_launch(&mut conn, focus_id, &[q], None)
        .await
        .unwrap();
}

/// A local ordinary step that produces `var`.
fn supply(var: &str) -> Row {
    (
        format!("supply_{var}"),
        None,
        json!({"routing_tier": 1, "task_type": "quick_response", "output_var": var,
               "prompt_template": "Say hello."}),
    )
}

fn ordinary_tail() -> Row {
    (
        "tail".to_owned(),
        None,
        json!({"routing_tier": 1, "task_type": "quick_response", "output_var": "tail_output",
               "prompt_template": "Say goodbye."}),
    )
}

/// The rows of a test Focus for a block: producers of its inputs, the block, and
/// (for a pure block, which may not be last) an ordinary tail.
fn rows_for(id: &str, ex: &Value, with_tail: bool) -> Vec<Row> {
    // A name the pass reads from the store (a bound name, items.id=895's artifacts
    // among them) is seeded at pass start, so no earlier row produces it, and a
    // row may not (LOAD rejects an output_var with a bound name).
    let mut rows: Vec<Row> = input_names(id, ex)
        .iter()
        .filter(|n| !crate::conductor::bound_spec::NAMES.contains(&n.as_str()))
        .map(|n| supply(n))
        .collect();
    rows.push(("the_block".to_owned(), Some(id.to_owned()), ex.clone()));
    if with_tail {
        rows.push(ordinary_tail());
    }
    rows
}

#[tokio::test]
async fn the_mirror_has_a_row_for_every_registered_block() {
    let env = setup().await;
    for id in REGISTERED_STABLE_IDS {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM building_blocks
             WHERE stable_id = ? AND invocation_mode = 'inline_composable'",
        )
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "{id}: no inline_composable mirror row");
    }
}

#[tokio::test]
async fn every_example_loads_through_the_real_loader_as_the_right_kind_of_step() {
    let env = setup().await;
    for id in REGISTERED_STABLE_IDS {
        let ex = example_of(id);
        let pure = block_kind(id) == Some(BlockKind::Pure);
        let render = block_kind(id) == Some(BlockKind::Render);
        // A model block expands to an ordinary step, so it may be the LAST row, which
        // the placement lint forbids for a pure block. A render block's output is the
        // starter, real content, so it may be last too (items.id=387).
        let focus_id = format!("admission-{id}");
        seed_focus(&env.pool, &focus_id, "local_only", &rows_for(id, &ex, pure)).await;
        // items.id=897: a model block needs the run's document type, so its Focus
        // declares the question (a fixed one: a preset).
        if MODEL_STABLE_IDS.contains(id) {
            seed_document_type_question(&env.pool, &focus_id, true).await;
        }

        let def = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_or_else(|e| panic!("{id}: its example Focus must load: {e}"))
            .unwrap();
        let step = def
            .steps
            .iter()
            .find(|s| s.action_id == "the_block")
            .unwrap();

        if pure {
            assert_eq!(step.block_stable_id.as_deref(), Some(*id));
            assert!(step.requirement.is_none());
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        } else if render {
            // items.id=387: a block row that carries its own template and declared
            // inputs, makes no call, is external to the lint and is no model step.
            assert_eq!(step.block_stable_id.as_deref(), Some(*id));
            assert!(step.is_render_block() && step.is_external_for_lint());
            assert!(step.requirement.is_none() && step.block_customization.is_some());
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
            assert_eq!(step.external_access_override, None);
            assert!(!step.requires_user_handoff);
            assert!(step.field_requirements.is_empty() && step.store_op.is_none());
            assert!(!step.prompt_template.is_empty());
            assert_eq!(
                step.output_var.as_deref(),
                output_var_of(id, &ex).as_deref()
            );
            let named: BTreeSet<String> =
                crate::conductor::executor::find_tokens(&step.prompt_template)
                    .into_iter()
                    .collect();
            assert!(
                named.iter().all(|n| step.declared_inputs.contains(n)),
                "{id}: the template names only declared inputs"
            );
        } else {
            // 7: an ordinary local step.
            assert_eq!(
                step.block_stable_id, None,
                "{id}: a model block keeps no block id"
            );
            assert!(step.block_customization.is_none());
            let req = step
                .requirement
                .as_ref()
                .unwrap_or_else(|| panic!("{id}: no requirement"));
            assert_eq!(req.block_stable_id, *id);
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
            assert_eq!(
                step.external_access_override,
                Some(ExternalAccess::LocalOnly)
            );
            assert!(!step.requires_user_handoff);
            // items.id=903: the only clauses are the block's own required-input ones.
            assert!(step.store_op.is_none() && step.guard.is_none());
            assert!(step.and_guards.iter().all(|g| {
                g.condition == crate::conductor::guard::Condition::Present
                    && crate::conductor::bound_spec::spec(&g.var).is_some_and(|s| {
                        s.presence == crate::conductor::bound_spec::Presence::Optional
                    })
            }));
            assert_eq!(
                step.output_var.as_deref(),
                output_var_of(id, &ex).as_deref()
            );
            assert!(!step.prompt_template.is_empty());
            assert!(matches!(
                step.step_type,
                StepType::Generate | StepType::VoiceTransform
            ));
            assert_eq!(
                step.guide_id,
                crate::conductor::model_block::MODEL_BLOCK_GUIDE_ID
            );
            // The template names exactly the declared inputs, and never the two names a
            // hosted block may not read.
            let named: BTreeSet<String> =
                crate::conductor::executor::find_tokens(&step.prompt_template)
                    .into_iter()
                    .collect();
            let declared: BTreeSet<String> = step.declared_inputs.iter().cloned().collect();
            assert_eq!(named, declared, "{id}");
            // items.id=903: pc-03 reads the user's own request and is local only for
            // that reason (READS_THE_REQUEST); no other block names it.
            assert!(
                (READS_THE_REQUEST.contains(id) || !named.contains("user_input"))
                    && !named.contains("persona_context"),
                "{id}"
            );
            // Its range is within the Focus ceiling and the block's own.
            assert!(req.range.highest_tier <= ExternalAccess::LocalOnly);
            // The output variable is what a later row can read.
            assert!(def
                .steps
                .iter()
                .all(|s| s.action_id != "tail" || s.output_var.is_some()));
        }
    }
}

#[tokio::test]
async fn a_model_block_may_not_load_above_local_while_external_blocks_are_not_allowed() {
    let env = setup().await;
    // This test describes the interim state: B10 flips the constant and replaces it.
    if EXTERNAL_MODEL_BLOCKS_ALLOWED {
        return;
    }
    for id in MODEL_STABLE_IDS {
        let mut ex = example_of(id);
        ex["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        ex["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        let focus_id = format!("external-{id}");
        seed_focus(
            &env.pool,
            &focus_id,
            "unrestricted",
            &rows_for(id, &ex, false),
        )
        .await;
        let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not allowed yet"), "{id}: {e}");
        assert!(e.contains(id) && e.contains("the_block"), "{e}");
    }
}

#[tokio::test]
async fn a_cataloged_block_without_a_registration_still_fails_at_load() {
    let env = setup().await;
    seed_focus(
        &env.pool,
        "no-handler",
        "local_only",
        &[("the_block".to_owned(), Some("cb-02".to_owned()), json!({}))],
    )
    .await;
    let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, "no-handler")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("no compiled-in handler"), "{e}");
}

/// items.id=894 B0: an out-of-range routing_tier in a database row is a LOAD error,
/// not a panic.
#[tokio::test]
async fn an_out_of_range_routing_tier_in_a_database_row_is_a_load_error() {
    let env = setup().await;
    for (n, bad) in [0u64, 4, 9].into_iter().enumerate() {
        let focus_id = format!("bad-tier-{n}");
        seed_focus(
            &env.pool,
            &focus_id,
            "local_only",
            &[(
                "step_a".to_owned(),
                None,
                json!({"routing_tier": bad, "task_type": "quick_response",
                       "output_var": "x", "prompt_template": "hi"}),
            )],
        )
        .await;
        let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("step_a") && e.contains(&format!("routing_tier value {bad}")),
            "{e}"
        );
    }
}

/// items.id=898: the database path rejects a present-but-unparsable step_type and
/// an unknown task_type, with the step's name, through the same builder as the
/// YAML path. An absent step_type still loads.
#[tokio::test]
async fn an_unknown_step_type_or_task_type_in_a_database_row_is_a_load_error() {
    let env = setup().await;
    let cases = [
        (
            json!({"routing_tier": 1, "task_type": "quick_response", "step_type": "generat",
                "output_var": "x", "prompt_template": "hi"}),
            Some("unknown step_type"),
        ),
        (
            json!({"routing_tier": 1, "task_type": "generic",
                "output_var": "x", "prompt_template": "hi"}),
            Some("unknown task_type"),
        ),
        (
            json!({"routing_tier": 1, "task_type": "", "output_var": "x",
                "prompt_template": "hi"}),
            Some("unknown task_type"),
        ),
        (
            json!({"routing_tier": 1, "task_type": "quick_response",
                "output_var": "x", "prompt_template": "hi"}),
            None,
        ),
    ];
    for (n, (custom, expect)) in cases.into_iter().enumerate() {
        let focus_id = format!("bad-type-{n}");
        seed_focus(
            &env.pool,
            &focus_id,
            "local_only",
            &[("step_a".to_owned(), None, custom)],
        )
        .await;
        let r =
            crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id).await;
        match expect {
            Some(text) => {
                let e = r.unwrap_err().to_string();
                assert!(e.contains("step_a") && e.contains(text), "{e}");
            }
            None => {
                r.unwrap().unwrap();
            }
        }
    }
}

#[tokio::test]
async fn a_model_block_with_a_wrong_input_name_fails_the_declared_inputs_lint() {
    // The producers are missing, so the declared names are not real: the existing
    // declared-inputs lint (items.id=879) rejects the expanded step.
    let env = setup().await;
    seed_focus(
        &env.pool,
        "no-producer",
        "local_only",
        &[(
            "the_block".to_owned(),
            Some("cb-12".to_owned()),
            example_of("cb-12"),
        )],
    )
    .await;
    let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, "no-producer")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("content_brief") || e.contains("sender_details"),
        "{e}"
    );
}

// ---------------------------------------------------------------------------
// 7. A pure block leaves the track alone and leaks nothing
// ---------------------------------------------------------------------------

#[test]
fn a_pure_block_does_not_change_the_track_and_leaks_no_input_text() {
    let marker = "ZX9-ADMISSION-MARKER";
    for id in PURE_STABLE_IDS {
        let ex = example_of(id);
        let h = block_handler(id).unwrap();
        let mut track = TaskTrack::new();
        track.add_step(TaskStep {
            action_id: "earlier".to_owned(),
            output_var: Some(ex["input_var"].as_str().unwrap().to_owned()),
            content: format!("{marker} {marker}"),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        let before = track.steps().len();
        let seeds = std::collections::HashMap::new();
        let inputs = crate::conductor::block_dispatch::BlockInputs {
            track: &track,
            seeds: &seeds,
        };
        let out = (h.run)(&ex, &inputs).unwrap();
        assert_eq!(
            track.steps().len(),
            before,
            "{id}: run must not mutate the track"
        );
        assert!(
            !out.content.contains(marker),
            "{id}: the verdict leaked the input text"
        );
        assert_eq!(Some(out.output_var), output_var_of(id, &ex));
    }
}

// ---------------------------------------------------------------------------
// 8. The hosted rule, for every model block
// ---------------------------------------------------------------------------

/// The key of the spec variable a model block's example reads that a hosted block
/// must not point at user_input or persona_context.
fn spec_var_key(id: &str) -> &'static str {
    match id {
        "cb-12" => "brief_var",
        "cb-13" => "input_var",
        "cb-14" => "draft_var",
        other => panic!("no hosted-rule probe for {other}"),
    }
}

/// items.id=903: the model blocks whose input IS the user's request (pc-03). The
/// hosted rule refuses every hosted range of theirs, so they are local only.
const READS_THE_REQUEST: &[&str] = &["pc-03"];

#[test]
fn a_block_that_reads_the_request_is_refused_any_hosted_range() {
    for id in READS_THE_REQUEST {
        let RegisteredBlock::Model(spec) = registered_block(id).unwrap() else {
            unreachable!()
        };
        let mut ex = example_of(id);
        ex["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        ex["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        let parsed = (spec.parse)(&ex, true).unwrap();
        let e = (spec.expand)(
            &parsed,
            &ExpandInput {
                action_id: "the_block",
                focus_ceiling: ExternalAccess::Unrestricted,
                settings: None,
                external_allowed: true,
                document_type: None,
                envelope_override: None,
            },
        )
        .unwrap_err();
        assert!(e.contains("{user_input}"), "{id}: {e}");
    }
}

#[test]
fn a_model_block_that_allows_hosted_api_names_neither_user_input_nor_persona_context() {
    for id in MODEL_STABLE_IDS
        .iter()
        .filter(|id| !READS_THE_REQUEST.contains(id))
    {
        let RegisteredBlock::Model(spec) = registered_block(id).unwrap() else {
            unreachable!()
        };
        let hosted = |mut ex: Value| {
            ex["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
            ex["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
            ex
        };
        let expand = |ex: &Value| {
            let parsed = (spec.parse)(ex, true)?;
            (spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ExternalAccess::Unrestricted,
                    settings: None,
                    external_allowed: true,
                    document_type: None,
                    envelope_override: None,
                },
            )
        };
        // As authored: allowed.
        assert!(expand(&hosted(example_of(id))).is_ok(), "{id}");

        for banned in ["user_input", "persona_context"] {
            let mut ex = hosted(example_of(id));
            let key = spec_var_key(id);
            let old = ex["spec"][key].as_str().unwrap().to_owned();
            ex["spec"][key] = json!(banned);
            let declared: Vec<Value> = ex["common"]["declared_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    if n == old.as_str() {
                        json!(banned)
                    } else {
                        n.clone()
                    }
                })
                .collect();
            ex["common"]["declared_inputs"] = json!(declared);
            let e = expand(&ex).unwrap_err();
            assert!(
                e.contains(&format!("{{{banned}}}")) && e.contains("hosted_api"),
                "{id}: {e}"
            );

            // The same name is fine in a block that cannot reach a hosted API.
            let mut local = example_of(id);
            let declared: Vec<Value> = local["common"]["declared_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    if n == old.as_str() {
                        json!(banned)
                    } else {
                        n.clone()
                    }
                })
                .collect();
            local["common"]["declared_inputs"] = json!(declared);
            local["spec"][key] = json!(banned);
            let parsed = (spec.parse)(&local, false).unwrap();
            assert!((spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ExternalAccess::LocalOnly,
                    settings: None,
                    external_allowed: false,
                    document_type: None,
                    envelope_override: None,
                },
            )
            .is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// 9. No key can switch a gate off, and the envelope only narrows
// ---------------------------------------------------------------------------

#[test]
fn every_configuration_key_is_pinned_and_none_names_a_gate() {
    let mut keys = BTreeSet::new();
    // A maximal example of each model block: every optional key set.
    let mut draft = crate::conductor::model_block::draft::example();
    draft["common"]["class_preference"] = json!("small");
    draft["spec"]["context_var"] = json!("writer_context");
    let voice = crate::conductor::model_block::voice::example();
    let revise = crate::conductor::model_block::revise::example();
    for mut ex in [draft, voice, revise] {
        ex["common"]["class_preference"] = json!("small");
        all_keys(&ex, &mut keys);
    }
    // items.id=387: the render block's own keys (template is the one new key).
    all_keys(&crate::conductor::handoff_render::example(), &mut keys);
    // The document type (its length_words map keys are data, not configuration),
    // the settings and the two override shapes.
    let reg = registry().unwrap();
    let mut t: DocumentType = reg.document_type("rma_letter").unwrap().clone();
    t.based_on = Some("x".to_owned());
    t.params.draft.top_p = Some(0.9);
    let mut tj = serde_json::to_value(&t).unwrap();
    tj["length_words"] = json!(null);
    all_keys(&tj, &mut keys);
    all_keys(
        &serde_json::to_value(Settings::clone(&t.settings)).unwrap(),
        &mut keys,
    );
    all_keys(
        &serde_json::to_value(SettingsPatch {
            directness: Some("a".into()),
            formality: Some("a".into()),
            pacing: Some("a".into()),
            tone: Some("a".into()),
            length: Some("a".into()),
            latitude: Some(crate::conductor::model_block::document_type::Latitude::Free),
        })
        .unwrap(),
        &mut keys,
    );
    let envelope = EnvelopeOverride {
        quality_floor: Some(crate::conductor::model_block::config::QualityFloor {
            min: crate::conductor::model_block::config::QualityBar::Basic,
            max: crate::conductor::model_block::config::QualityBar::High,
        }),
        highest_tier: Some(ExternalAccess::LocalOnly),
        destinations: Some(vec![]),
        declared_inputs: Some(vec![]),
    };
    all_keys(&serde_json::to_value(&envelope).unwrap(), &mut keys);

    let pinned: BTreeSet<String> = PINNED_KEYS.iter().map(|k| (*k).to_owned()).collect();
    assert_eq!(
        keys, pinned,
        "a configuration key was added or removed: review it against FORBIDDEN_SUBSTRINGS and \
         update PINNED_KEYS"
    );
    for k in &keys {
        for bad in FORBIDDEN_SUBSTRINGS {
            assert!(!k.contains(bad), "configuration key '{k}' names '{bad}'");
        }
    }
}

#[test]
fn an_empty_override_is_the_identity_and_the_focus_ceiling_only_narrows_every_example() {
    for id in MODEL_STABLE_IDS {
        let RegisteredBlock::Model(spec) = registered_block(id).unwrap() else {
            unreachable!()
        };
        let parsed = (spec.parse)(&example_of(id), false).unwrap();
        let env: Envelope = parsed.common.envelope();
        assert_eq!(
            tighten(&env, &EnvelopeOverride::default()).as_ref(),
            Ok(&env)
        );
        for ceiling in [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ] {
            let step = (spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ceiling,
                    settings: None,
                    external_allowed: false,
                    document_type: None,
                    envelope_override: None,
                },
            )
            .unwrap();
            assert!(step.routing_tier <= ceiling.min(env.highest_tier));
            let req = step.requirement.unwrap();
            assert!(req.range.highest_tier <= env.highest_tier);
            assert!(req
                .range
                .destinations
                .iter()
                .all(|d| env.destinations.contains(d)));
        }
    }
}

/// items.id=895: the four artifact names are reserved. No shipped example of any
/// block may use a bound name as its output variable (the loader would refuse the
/// Focus), and the draft example's brief is read as the content_brief artifact.
#[test]
fn no_example_uses_a_bound_name_as_its_output_variable() {
    for id in REGISTERED_STABLE_IDS {
        let ex = example_of(id);
        if let Some(v) = ex["common"]["output_var"].as_str() {
            assert!(
                !crate::conductor::bound_spec::NAMES.contains(&v),
                "{id}: output_var '{v}' is a bound name"
            );
        }
    }
    let draft = example_of("cb-12");
    assert_eq!(draft["spec"]["brief_var"], "content_brief");
    assert!(crate::conductor::bound_spec::ARTIFACT_NAMES.contains(&"content_brief"));
}

// ---------------------------------------------------------------------------
// 10: the resolver can serve every shipped block and document type (items.id=896)
// ---------------------------------------------------------------------------

/// Shipped document types with NO rated local cell in the shipped capability data.
/// A step of such a type resolves to TooWeak (every model not measured) until a
/// catalog rates it. Naming it here is the decision that this is known. Empty since
/// items.id=899: the calibration run rated creative_story, so every shipped type has
/// a cell. A new shipped type forces a decision here again.
const UNRATED_SHIPPED_TYPES: &[&str] = &[];

fn shipped_capabilities() -> Vec<crate::provider_catalog::format::CapabilityDoc> {
    crate::provider_catalog::format::parse_catalog(
        crate::provider_catalog::BASELINE_CATALOG_JSON.as_bytes(),
    )
    .expect("the baseline catalog parses")
    .capabilities
}

#[test]
fn every_shipped_document_type_is_rated_or_named_as_unrated() {
    use crate::conductor::model_capability::CapabilitySubject;
    let reg = crate::conductor::model_block::document_type::registry().unwrap();
    let cells = shipped_capabilities();
    for t in &reg.types {
        let rated = cells.iter().any(|c| {
            c.subject == CapabilitySubject::Local && c.profile.as_deref() == Some(t.id.as_str())
        });
        let named = UNRATED_SHIPPED_TYPES.contains(&t.id.as_str());
        assert!(
            rated || named,
            "document type '{}' has no rated local cell: rate it in the capability data or \
             name it in UNRATED_SHIPPED_TYPES",
            t.id
        );
        assert!(
            !(rated && named),
            "document type '{}' is rated now: remove it from UNRATED_SHIPPED_TYPES",
            t.id
        );
    }
    for n in UNRATED_SHIPPED_TYPES {
        assert!(
            reg.document_type(n).is_some(),
            "'{n}' is not a shipped type"
        );
    }
}

/// Task types of a registered model block with NO cell in the shipped capability
/// data. A step of such a block resolves to TooWeak until a catalog rates the task
/// type. Empty since items.id=899: the harness rated quick_response (the voice
/// block's task type) through the shipped cb-13 block. A new block task type forces
/// a decision here again.
const UNRATED_BLOCK_TASK_TYPES: &[&str] = &[];

#[test]
fn every_model_block_task_type_has_a_cell_or_is_named_as_unrated() {
    let cells = shipped_capabilities();
    let mut used: BTreeSet<String> = BTreeSet::new();
    for id in crate::conductor::model_block::MODEL_STABLE_IDS {
        let ex = example_of(id);
        let task = ex["common"]["need"]["task_type"]
            .as_str()
            .expect("a task type")
            .to_owned();
        let rated = cells.iter().any(|c| c.task_type.as_str() == task);
        let named = UNRATED_BLOCK_TASK_TYPES.contains(&task.as_str());
        assert!(
            rated || named,
            "{id}: no capability cell for task type '{task}': rate it or name it in \
             UNRATED_BLOCK_TASK_TYPES"
        );
        assert!(
            !(rated && named),
            "task type '{task}' is rated now: remove it from UNRATED_BLOCK_TASK_TYPES"
        );
        used.insert(task);
    }
    for n in UNRATED_BLOCK_TASK_TYPES {
        assert!(
            used.contains(*n),
            "'{n}' is named as unrated but no model block uses it"
        );
    }
}

#[test]
fn every_shipped_local_cell_names_a_curated_tag() {
    use crate::conductor::model_capability::CapabilitySubject;
    for c in shipped_capabilities() {
        if c.subject == CapabilitySubject::Local {
            assert!(
                crate::providers::curated_models::is_curated_local_model_tag(&c.model_id),
                "'{}' is not a curated local tag",
                c.model_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 11: launch questions and mode profiles (items.id=897, decisions.id=948)
// ---------------------------------------------------------------------------
//
//  * a Focus's launch config is read only through the validating reader, and LOAD
//    refuses one that fails validate() (a default that is not an option, among
//    others) and any step that uses a launch name wrongly;
//  * a Focus with a model block must declare a document_type question, because the
//    resolver reads its profile only from that seed (rider of items.id=896);
//  * every shipped launch config validates, and every question ships a default that
//    is one of its options;
//  * a mode profile is an EnvelopeOverride, whose four keys are pinned: it can only
//    narrow, and no key can switch a gate off.

/// A mode question (launch_mode) with the options quick and careful, each with a
/// profile, on `focus_id`. quick narrows to local only, careful raises the floor.
async fn seed_mode_question(pool: &sqlx::SqlitePool, focus_id: &str) {
    let q = launch_question(
        "how",
        "launch_mode",
        "mode",
        true,
        "quick",
        "How?",
        &[
            (
                "quick",
                Some(r#"{"highest_tier":"local_only","destinations":["local"]}"#),
            ),
            (
                "careful",
                Some(r#"{"quality_floor":{"min":"standard","max":"high"}}"#),
            ),
        ],
    );
    let mut conn = pool.acquire().await.unwrap();
    crate::persistence::focus_composition_store::write_launch(&mut conn, focus_id, &[q], None)
        .await
        .unwrap();
}

async fn guard_row(
    pool: &sqlx::SqlitePool,
    focus_id: &str,
    action: &str,
    op: &str,
    var: &str,
    value: &str,
) {
    sqlx::query(
        "UPDATE focus_block_compositions SET guard_var = ?, guard_op = ?, guard_value = ?
          WHERE id = ?",
    )
    .bind(var)
    .bind(op)
    .bind(value)
    .bind(format!("{focus_id}:{action}"))
    .execute(pool)
    .await
    .unwrap();
}

async fn load_text(pool: &sqlx::SqlitePool, focus_id: &str) -> Result<(), String> {
    crate::conductor::lifecycle::load_focus_definition_from_db(pool, focus_id)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn local_step(id: &str, template: &str) -> Row {
    (
        id.to_owned(),
        None,
        json!({"routing_tier": 1, "task_type": "quick_response", "output_var": format!("{id}_out"),
               "prompt_template": template}),
    )
}

#[tokio::test]
async fn a_focus_with_a_model_block_must_declare_a_document_type_question() {
    let env = setup().await;
    let ex = example_of("cb-12");
    // No question: refused at LOAD, naming the reason.
    seed_focus(
        &env.pool,
        "dt-none",
        "local_only",
        &rows_for("cb-12", &ex, false),
    )
    .await;
    let e = load_text(&env.pool, "dt-none").await.unwrap_err();
    assert!(e.contains("document_type") && e.contains("too weak"), "{e}");
    // A fixed question (a preset) and an asked question both satisfy it.
    seed_focus(
        &env.pool,
        "dt-fixed",
        "local_only",
        &rows_for("cb-12", &ex, false),
    )
    .await;
    seed_document_type_question(&env.pool, "dt-fixed", true).await;
    load_text(&env.pool, "dt-fixed").await.unwrap();
    seed_focus(
        &env.pool,
        "dt-asked",
        "local_only",
        &rows_for("cb-12", &ex, false),
    )
    .await;
    seed_document_type_question(&env.pool, "dt-asked", false).await;
    load_text(&env.pool, "dt-asked").await.unwrap();
    // A question on another slot does not.
    seed_focus(
        &env.pool,
        "dt-other",
        "local_only",
        &rows_for("cb-12", &ex, false),
    )
    .await;
    seed_mode_question(&env.pool, "dt-other").await;
    let e = load_text(&env.pool, "dt-other").await.unwrap_err();
    assert!(e.contains("document_type"), "{e}");
}

#[tokio::test]
async fn a_step_naming_a_launch_slot_with_no_question_fails_to_load() {
    let env = setup().await;
    seed_focus(
        &env.pool,
        "slot-none",
        "local_only",
        &[local_step("a", "mode: {launch_mode}")],
    )
    .await;
    let e = load_text(&env.pool, "slot-none").await.unwrap_err();
    assert!(
        e.contains("launch_mode") && e.contains("no launch question"),
        "{e}"
    );
    // With the question it loads (a local step may name it freely).
    seed_focus(
        &env.pool,
        "slot-ok",
        "local_only",
        &[local_step("a", "mode: {launch_mode}")],
    )
    .await;
    seed_mode_question(&env.pool, "slot-ok").await;
    load_text(&env.pool, "slot-ok").await.unwrap();
    // run_change needs no question.
    seed_focus(
        &env.pool,
        "chg",
        "local_only",
        &[local_step("a", "change: {run_change}")],
    )
    .await;
    load_text(&env.pool, "chg").await.unwrap();
}

#[tokio::test]
async fn a_guard_literal_outside_the_questions_options_fails_to_load() {
    let env = setup().await;
    // Rows a (eq quick) and b (ne quick) cover every value, so the guard analysis
    // is satisfied and only the literal is in question.
    for (id, literal, ok) in [("g-ok", "quick", true), ("g-typo", "quik", false)] {
        seed_focus(
            &env.pool,
            id,
            "local_only",
            &[local_step("a", "x"), local_step("b", "y")],
        )
        .await;
        seed_mode_question(&env.pool, id).await;
        guard_row(&env.pool, id, "a", "eq", "launch_mode", literal).await;
        guard_row(&env.pool, id, "b", "ne", "launch_mode", literal).await;
        let r = load_text(&env.pool, id).await;
        if ok {
            r.unwrap_or_else(|e| panic!("{id}: {e}"));
        } else {
            let e = r.unwrap_err();
            assert!(
                e.contains("not one of the options") && e.contains("'quik'"),
                "{e}"
            );
        }
    }
}

#[tokio::test]
async fn a_launch_config_that_fails_validate_fails_the_load() {
    let env = setup().await;
    seed_focus(&env.pool, "bad-cfg", "local_only", &[local_step("a", "x")]).await;
    seed_mode_question(&env.pool, "bad-cfg").await;
    // A default that is not one of the options.
    sqlx::query(
        "UPDATE focus_launch_questions SET default_option = 'novel' WHERE focus_id = 'bad-cfg'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let e = load_text(&env.pool, "bad-cfg").await.unwrap_err();
    assert!(e.contains("default 'novel'"), "{e}");
    // A mode with no profile.
    sqlx::query(
        "UPDATE focus_launch_questions SET default_option = 'quick' WHERE focus_id = 'bad-cfg'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM focus_mode_profiles WHERE focus_id = 'bad-cfg' AND option_id = 'careful'",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let e = load_text(&env.pool, "bad-cfg").await.unwrap_err();
    assert!(e.contains("has no profile"), "{e}");
}

#[tokio::test]
async fn every_shipped_launch_config_validates_and_every_default_is_an_option() {
    let env = setup().await;
    let reg = registry().unwrap();
    let ids = crate::conductor::launch_config::declared_focus_ids(&env.pool)
        .await
        .unwrap();
    // decisions.id=948 point 6: none shipped in items.id=897; items.id=903 ships the
    // Writing Assistant's. Whatever ships later is held to the rules here.
    assert_eq!(
        ids,
        ["writing-assistant"],
        "unexpected shipped launch questions"
    );
    for id in ids {
        let c = crate::conductor::launch_config::read_validated(&env.pool, &id, reg)
            .await
            .unwrap_or_else(|e| panic!("{id}: {e}"))
            .unwrap();
        for q in &c.questions {
            assert!(q.option(&q.default_option).is_some(), "{id}/{}", q.id);
        }
    }
}

/// A mode profile is an EnvelopeOverride: exactly four keys, each a tighten-only
/// thing. A new key must be reviewed here, and none may name a gate, a consent, a
/// spend limit or an approval.
#[test]
fn a_mode_profile_has_exactly_the_four_envelope_keys_and_none_names_a_gate() {
    use crate::conductor::model_block::config::{Destination, QualityBar, QualityFloor};
    let all = EnvelopeOverride {
        quality_floor: Some(QualityFloor {
            min: QualityBar::Basic,
            max: QualityBar::High,
        }),
        highest_tier: Some(ExternalAccess::LocalOnly),
        destinations: Some(vec![Destination::Local]),
        declared_inputs: Some(vec!["a".to_owned()]),
    };
    let v = serde_json::to_value(&all).unwrap();
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "declared_inputs",
            "destinations",
            "highest_tier",
            "quality_floor"
        ])
    );
    for k in &keys {
        for banned in FORBIDDEN_SUBSTRINGS {
            assert!(!k.contains(banned), "{k} names '{banned}'");
        }
    }
    // And a profile naming anything else does not parse.
    for bad in [
        r#"{"gate1": false}"#,
        r#"{"approve_hosted": true}"#,
        r#"{"spend_limit": 100}"#,
        r#"{"settings": {}}"#,
    ] {
        assert!(
            serde_json::from_str::<EnvelopeOverride>(bad).is_err(),
            "{bad}"
        );
    }
}

/// The launch names are an exact, reviewed list: adding one is a design decision.
#[test]
fn the_launch_names_are_an_exact_reviewed_list() {
    assert_eq!(
        crate::conductor::bound_spec::LAUNCH_NAMES,
        [
            "document_type",
            "launch_mode",
            "launch_answer_1",
            "launch_answer_2",
            "launch_answer_3",
            "run_change"
        ]
    );
    assert_eq!(crate::conductor::launch_config::MAX_QUESTIONS, 5);
}

// ---------------------------------------------------------------------------
// 12: the run-start step, end to end (items.id=897 stage 3, decisions.id=948)
// ---------------------------------------------------------------------------
//
// A real FocusRun loads a Focus from shared.db whose model block has launch
// questions, reads the chat's answers from a real persona outputs.db, re-expands the
// block, and seeds the resolution. What is real: the migrations, the loader, the
// answer store, FocusRun::load(), seed_pass(). Nothing calls a model.

use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::launch_config::{read_validated, validate_submission};
use crate::conductor::lifecycle::{FocusRun, LifecycleError};
use crate::conductor::run_settings::LaunchResolution;
use crate::persistence::launch_answer_store::{put_answers, PutError};
use crate::persistence::output_store::open_outputs_db;

const USER: &str = "launch-user";
const PERSONA: &str = "launch-persona";
const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const CHAT: &str = "launch-chat";

/// A Focus with a cb-12 draft block, an asked document_type question (rma_letter by
/// default, cover_letter the other option) and a mode question (quick by default,
/// careful the other).
async fn launch_focus(env: &TestEnv, id: &str) {
    seed_focus(
        &env.pool,
        id,
        "local_only",
        &rows_for("cb-12", &example_of("cb-12"), false),
    )
    .await;
    seed_document_type_question(&env.pool, id, false).await;
    seed_mode_question(&env.pool, id).await;
}

fn run_for(env: &TestEnv, focus: &str, context_key: Option<&str>) -> FocusRun {
    let mut run = FocusRun::new(
        USER.to_owned(),
        PERSONA.to_owned(),
        focus.to_owned(),
        env.pool.clone(),
        std::sync::Arc::new(ConductorScheduler::new()),
        String::new(),
        false,
        Some(KEY.to_owned()),
        None,
        false,
        std::collections::HashSet::new(),
        None,
    );
    run.context_key = context_key.map(str::to_owned);
    run
}

async fn submit(
    env: &TestEnv,
    focus: &str,
    answers: &[(&str, &str)],
    run_change: Option<&str>,
) -> Result<(), PutError> {
    let reg = registry().unwrap();
    let cfg = read_validated(&env.pool, focus, reg)
        .await
        .unwrap()
        .unwrap();
    let v = validate_submission(&cfg, answers, run_change, reg).unwrap();
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    put_answers(&mut conn, CHAT, focus, &v).await
}

fn block_of(run: &FocusRun) -> crate::conductor::tokens::ActionDefinition {
    run.focus_def
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .find(|s| s.action_id == "the_block")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn a_run_with_no_answers_resolves_every_question_to_its_default() {
    let env = setup().await;
    launch_focus(&env, "rs-default").await;
    let mut run = run_for(&env, "rs-default", Some(CHAT));
    run.load().await.unwrap();
    let l = &run.launch;
    assert_eq!(l.values["document_type"], "rma_letter");
    assert_eq!(l.values["launch_mode"], "quick");
    assert_eq!(l.mode.as_deref(), Some("quick"));
    // The default mode narrows to local only, through tighten().
    let req = block_of(&run).requirement.unwrap();
    assert_eq!(req.range.highest_tier, ExternalAccess::LocalOnly);
    assert_eq!(
        req.range.destinations,
        vec![crate::conductor::model_block::config::Destination::Local]
    );
}

#[tokio::test]
async fn the_answers_change_the_expansion_the_mode_profile_and_the_seeds() {
    use crate::conductor::model_block::config::QualityBar;
    let env = setup().await;
    launch_focus(&env, "rs-answers").await;
    let mut before = run_for(&env, "rs-answers", Some(CHAT));
    before.load().await.unwrap();
    let default_template = block_of(&before).prompt_template;
    let default_floor = block_of(&before).requirement.unwrap().range.quality_floor;

    submit(
        &env,
        "rs-answers",
        &[("how", "careful"), ("kind", "cover_letter")],
        Some(r#"{"length":"long"}"#),
    )
    .await
    .unwrap();
    let mut run = run_for(&env, "rs-answers", Some(CHAT));
    run.load().await.unwrap();
    let step = block_of(&run);
    // The selected type replaced the row's own (rma_letter): a different template,
    // with the long range of the cover letter (the per-run change, layer 3).
    assert_ne!(step.prompt_template, default_template);
    let long = registry()
        .unwrap()
        .document_type("cover_letter")
        .unwrap()
        .length_words["long"];
    assert!(
        step.prompt_template.contains(&format!(
            "between {} and {} words",
            long.min_words, long.max_words
        )),
        "{}",
        step.prompt_template
    );
    // The careful mode raised the floor (only ever up).
    let floor = step.requirement.unwrap().range.quality_floor;
    assert!(floor.min >= default_floor.min && floor.min == QualityBar::Standard);

    // The seeds are the resolution, and the resolver's profile equals the answer.
    let def = run.focus_def.as_ref().unwrap();
    let row_vars: Vec<Option<String>> = def.steps.iter().map(|s| s.output_var.clone()).collect();
    let tokens = crate::conductor::seeding::seed_pass(
        USER,
        PERSONA,
        KEY,
        Some(CHAT),
        "rs-answers",
        &def.steps,
        &row_vars,
        &run.launch,
    )
    .await
    .unwrap();
    assert_eq!(tokens.seeds["document_type"], "cover_letter");
    let shipped = registry().unwrap();
    assert_eq!(
        crate::conductor::model_resolver::profile_for_document_type(
            &tokens.seeds[crate::conductor::model_resolver_input::DOCUMENT_TYPE_NAME],
            shipped,
            &[]
        )
        .as_deref(),
        Some("cover_letter")
    );
    // The launch names are restricted: no step receives one unless it names it.
    assert!(tokens.restricted.contains("document_type"));
}

#[tokio::test]
async fn a_stored_answer_outside_the_options_fails_the_run_and_no_default_replaces_it() {
    let env = setup().await;
    launch_focus(&env, "rs-stale").await;
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    sqlx::query(
        "INSERT INTO session_variables
           (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
         VALUES (?, 'rs-stale', 'launch_mode', 'gone_mode', 1, 'r', 'now', 'now')",
    )
    .bind(CHAT)
    .execute(&mut conn)
    .await
    .unwrap();
    let mut run = run_for(&env, "rs-stale", Some(CHAT));
    let e = run.load().await.unwrap_err();
    assert!(matches!(e, LifecycleError::LaunchAnswers(_)), "{e}");
    let text = e.to_string();
    assert!(
        text.contains("how") && !text.contains("gone_mode"),
        "{text}"
    );
}

#[tokio::test]
async fn a_stored_run_change_that_does_not_parse_fails_the_run() {
    let env = setup().await;
    launch_focus(&env, "rs-badchange").await;
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    sqlx::query(
        "INSERT INTO session_variables
           (context_key, focus_id, name, value, version, written_by_run, created_at, updated_at)
         VALUES (?, 'rs-badchange', 'run_change', '{\"highest_tier\":\"unrestricted\"}', 1, 'r', 'now', 'now')",
    )
    .bind(CHAT)
    .execute(&mut conn)
    .await
    .unwrap();
    let e = run_for(&env, "rs-badchange", Some(CHAT))
        .load()
        .await
        .unwrap_err();
    assert!(matches!(e, LifecycleError::LaunchAnswers(_)), "{e}");
}

#[tokio::test]
async fn pre_latch_answers_still_apply_and_a_latched_submit_is_refused_with_its_own_code() {
    let env = setup().await;
    launch_focus(&env, "rs-latch").await;
    submit(&env, "rs-latch", &[("kind", "cover_letter")], None)
        .await
        .unwrap();
    let mut conn = open_outputs_db(USER, PERSONA, KEY).await.unwrap();
    crate::persistence::session_store::latch_write_suppressed(&mut conn, CHAT, "rs-latch", "run-1")
        .await
        .unwrap();
    // The latch deleted nothing and reads are not gated: the answer still applies.
    let mut run = run_for(&env, "rs-latch", Some(CHAT));
    run.load().await.unwrap();
    assert_eq!(run.launch.values["document_type"], "cover_letter");
    // A new submit is refused whole with the one-time code, and nothing changed.
    let e = submit(&env, "rs-latch", &[("kind", "rma_letter")], None)
        .await
        .unwrap_err();
    assert_eq!(e, PutError::OneTimeChat);
    let mut again = run_for(&env, "rs-latch", Some(CHAT));
    again.load().await.unwrap();
    assert_eq!(again.launch.values["document_type"], "cover_letter");
    // A latched chat that never answered uses the defaults.
    let mut fresh = run_for(&env, "rs-latch", Some("another-latched-chat"));
    crate::persistence::session_store::latch_write_suppressed(
        &mut conn,
        "another-latched-chat",
        "rs-latch",
        "run-2",
    )
    .await
    .unwrap();
    fresh.load().await.unwrap();
    assert_eq!(fresh.launch.values["document_type"], "rma_letter");
}

#[tokio::test]
async fn a_resumed_run_of_a_focus_with_an_asked_question_fails_closed_with_its_own_error() {
    let env = setup().await;
    launch_focus(&env, "rs-resume").await;
    let mut run = run_for(&env, "rs-resume", None);
    run.rehydrated = true;
    let e = run.load().await.unwrap_err();
    assert!(
        matches!(&e, LifecycleError::LaunchAnswersUnavailableOnResume(f) if f == "rs-resume"),
        "{e}"
    );
    // A Focus whose questions are all fixed (a preset) has nothing to ask, so its
    // resumed run loads with the defaults.
    seed_focus(
        &env.pool,
        "rs-resume-fixed",
        "local_only",
        &rows_for("cb-12", &example_of("cb-12"), false),
    )
    .await;
    seed_document_type_question(&env.pool, "rs-resume-fixed", true).await;
    let mut fixed = run_for(&env, "rs-resume-fixed", None);
    fixed.rehydrated = true;
    fixed.load().await.unwrap();
    assert_eq!(fixed.launch.values["document_type"], "rma_letter");
    // A run with no chat that was not resumed (submit_focus_run) uses the defaults.
    let mut nochat = run_for(&env, "rs-resume", None);
    nochat.load().await.unwrap();
    assert_eq!(nochat.launch.values["launch_mode"], "quick");
}

/// items.id=888 stage 4: a resumed run that carries its chat (focus_runs.context_key,
/// outputs_019) reads the chat's stored answers, exactly as a fresh run of that chat does,
/// instead of failing with LaunchAnswersUnavailableOnResume. A resumed run with no chat
/// still fails closed (the test above).
#[tokio::test]
async fn a_resumed_run_with_its_chat_reads_the_stored_answers() {
    let env = setup().await;
    launch_focus(&env, "rs-resume-chat").await;
    submit(&env, "rs-resume-chat", &[("kind", "cover_letter")], None)
        .await
        .unwrap();
    let mut resumed = run_for(&env, "rs-resume-chat", Some(CHAT));
    resumed.rehydrated = true;
    resumed.load().await.unwrap();
    assert_eq!(resumed.launch.values["document_type"], "cover_letter");
}

#[tokio::test]
async fn a_focus_with_no_launch_config_and_no_model_block_is_untouched() {
    let env = setup().await;
    seed_focus(
        &env.pool,
        "rs-plain",
        "local_only",
        &[local_step("a", "hello")],
    )
    .await;
    let mut run = run_for(&env, "rs-plain", Some(CHAT));
    run.load().await.unwrap();
    assert_eq!(run.launch, LaunchResolution::default());
    assert!(run.focus_def.as_ref().unwrap().model_origins.is_empty());
    // Even resumed: nothing is asked.
    let mut resumed = run_for(&env, "rs-plain", None);
    resumed.rehydrated = true;
    resumed.load().await.unwrap();
}

#[tokio::test]
async fn a_row_guard_survives_the_re_expansion() {
    let env = setup().await;
    launch_focus(&env, "rs-guard").await;
    let mut run = run_for(&env, "rs-guard", Some(CHAT));
    run.load().await.unwrap();
    let def = run.focus_def.as_mut().unwrap();
    let step = def
        .steps
        .iter_mut()
        .find(|s| s.action_id == "the_block")
        .unwrap();
    step.guard = Some(crate::conductor::guard::Guard {
        var: "launch_mode".to_owned(),
        condition: crate::conductor::guard::Condition::Eq("quick".to_owned()),
    });
    let resolved = crate::conductor::run_settings::resolve(
        read_validated(&env.pool, "rs-guard", registry().unwrap())
            .await
            .unwrap()
            .as_ref(),
        &Default::default(),
        None,
        registry().unwrap(),
    )
    .unwrap();
    crate::conductor::run_settings::reexpand(def, &resolved).unwrap();
    let step = def
        .steps
        .iter()
        .find(|s| s.action_id == "the_block")
        .unwrap();
    assert!(step.guard.is_some(), "the guard is kept");
}

// -- the trial expansion refuses what cannot be applied ----------------------

#[tokio::test]
async fn a_mode_profile_that_cannot_be_applied_to_a_model_block_refuses_the_focus_at_load() {
    let env = setup().await;
    // (a) removes a declared input the block's template names.
    // (b) leaves no destination (the block reaches local only).
    for (id, envelope, expect) in [
        (
            "trial-input",
            r#"{"declared_inputs":["content_brief"]}"#,
            "remove a declared input",
        ),
        (
            "trial-dest",
            r#"{"destinations":["hosted_api"]}"#,
            "no destination",
        ),
    ] {
        seed_focus(
            &env.pool,
            id,
            "local_only",
            &rows_for("cb-12", &example_of("cb-12"), false),
        )
        .await;
        seed_document_type_question(&env.pool, id, true).await;
        seed_mode_question(&env.pool, id).await;
        sqlx::query("UPDATE focus_mode_profiles SET envelope = ? WHERE focus_id = ? AND option_id = 'careful'")
            .bind(envelope)
            .bind(id)
            .execute(&env.pool)
            .await
            .unwrap();
        let e = load_text(&env.pool, id).await.unwrap_err();
        assert!(e.contains("careful") && e.contains(expect), "{id}: {e}");
    }
    // And a good one loads.
    launch_focus(&env, "trial-ok").await;
    load_text(&env.pool, "trial-ok").await.unwrap();
}

// -- the seeds -----------------------------------------------------------------

#[tokio::test]
async fn a_caller_that_supplies_no_resolution_still_fails_closed() {
    let env = setup().await;
    launch_focus(&env, "rs-seed").await;
    let mut run = run_for(&env, "rs-seed", Some(CHAT));
    run.load().await.unwrap();
    let def = run.focus_def.as_ref().unwrap();
    let row_vars: Vec<Option<String>> = def.steps.iter().map(|s| s.output_var.clone()).collect();
    // The model block needs document_type (the rider), and an empty resolution has
    // none: the pass fails closed, naming the launch name.
    let e = crate::conductor::seeding::seed_pass(
        USER,
        PERSONA,
        KEY,
        Some(CHAT),
        "rs-seed",
        &def.steps,
        &row_vars,
        &LaunchResolution::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e,
        crate::conductor::seeding::SeedError::LaunchNotResolved("document_type")
    );
    // With the run's own resolution it seeds.
    crate::conductor::seeding::seed_pass(
        USER,
        PERSONA,
        KEY,
        Some(CHAT),
        "rs-seed",
        &def.steps,
        &row_vars,
        &run.launch,
    )
    .await
    .unwrap();
}
